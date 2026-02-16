use std::io::{self, Write};
use std::marker::PhantomData;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use tracing::subscriber::Interest;
use tracing::{Event, Metadata, Subscriber};
use tracing_subscriber::filter::EnvFilter;
use tracing_subscriber::fmt::format::{DefaultFields, Format, Full};
use tracing_subscriber::fmt::{self, FormatFields, MakeWriter};
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;

use crate::capture::{CaptureMakeWriter, return_captured, take_captured};
use crate::reservoir::Reservoir;

pub(crate) struct State {
    pub(crate) bucket_index: u64,
    pub(crate) seq: u64,
    pub(crate) reservoirs: Vec<Reservoir<(u64, Vec<u8>)>>,
}

/// Shared handle for reading layer event counters.
///
/// Returned by [`SamplingLayerBuilder::build`](crate::SamplingLayerBuilder::build).
/// All counts are cumulative since layer creation.
#[derive(Clone)]
pub struct Stats {
    received: std::sync::Arc<AtomicU64>,
    sampled: std::sync::Arc<AtomicU64>,
    dropped: std::sync::Arc<AtomicU64>,
}

impl Stats {
    pub(crate) fn new() -> Self {
        Self {
            received: std::sync::Arc::new(AtomicU64::new(0)),
            sampled: std::sync::Arc::new(AtomicU64::new(0)),
            dropped: std::sync::Arc::new(AtomicU64::new(0)),
        }
    }

    /// Events that matched at least one filter.
    pub fn received(&self) -> u64 {
        self.received.load(Ordering::Relaxed)
    }

    /// Events that were kept in a reservoir.
    pub fn sampled(&self) -> u64 {
        self.sampled.load(Ordering::Relaxed)
    }

    /// Events that were dropped after failing to enter any reservoir.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

/// A [`tracing_subscriber::Layer`] that samples events into time-bucketed reservoirs.
///
/// Uses `tracing_subscriber::fmt::Layer` internally for event formatting.
/// Construct via [`SamplingLayer::builder()`](crate::SamplingLayerBuilder).
pub struct SamplingLayer<
    S,
    N = DefaultFields,
    E = Format<Full>,
    W: for<'a> MakeWriter<'a> = fn() -> io::Stderr,
> {
    pub(crate) filters: Vec<EnvFilter>,
    pub(crate) state: Mutex<State>,
    pub(crate) bucket_duration_ns: u64,
    pub(crate) writer: W,
    pub(crate) fmt_layer: fmt::Layer<S, N, E, CaptureMakeWriter>,
    pub(crate) stats: Stats,
    pub(crate) _subscriber: PhantomData<fn(S)>,
}

fn current_bucket_index(bucket_duration_ns: u64) -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64
        / bucket_duration_ns
}

impl<S, N, E, W: for<'a> MakeWriter<'a>> SamplingLayer<S, N, E, W> {
    fn drain_all(state: &mut State) -> Vec<(u64, Vec<u8>)> {
        let mut events: Vec<_> = state
            .reservoirs
            .iter_mut()
            .flat_map(|r| r.drain())
            .collect();
        events.sort_unstable_by_key(|(seq, _)| *seq);
        events
    }

    fn write_events(&self, events: &[(u64, Vec<u8>)]) {
        if events.is_empty() {
            return;
        }
        let mut writer = self.writer.make_writer();
        for (_, buf) in events {
            let _ = writer.write_all(buf);
        }
    }

    /// Drain all reservoirs and write their contents immediately.
    pub fn flush(&self) {
        let events = Self::drain_all(&mut self.state.lock().unwrap());
        self.write_events(&events);
    }
}

impl<S, N, E, W: for<'a> MakeWriter<'a>> Drop for SamplingLayer<S, N, E, W> {
    fn drop(&mut self) {
        if let Ok(mut state) = self.state.lock() {
            let events = Self::drain_all(&mut state);
            self.write_events(&events);
        }
    }
}

impl<S, N, E, W> tracing_subscriber::Layer<S> for SamplingLayer<S, N, E, W>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'writer> FormatFields<'writer> + 'static,
    E: fmt::FormatEvent<S, N> + 'static,
    W: for<'a> MakeWriter<'a> + 'static,
{
    fn register_callsite(&self, meta: &'static Metadata<'static>) -> Interest {
        for filter in &self.filters {
            let interest =
                <EnvFilter as tracing_subscriber::Layer<S>>::register_callsite(filter, meta);
            if interest.is_sometimes() || interest.is_always() {
                return Interest::sometimes();
            }
        }
        Interest::never()
    }

    fn enabled(&self, meta: &Metadata<'_>, ctx: Context<'_, S>) -> bool {
        self.filters.iter().any(|filter| {
            <EnvFilter as tracing_subscriber::Layer<S>>::enabled(filter, meta, ctx.clone())
        })
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let meta = event.metadata();
        let bucket_index = current_bucket_index(self.bucket_duration_ns);

        let flushed = {
            let mut state = self.state.lock().unwrap();
            if state.bucket_index < bucket_index {
                state.bucket_index = bucket_index;
                Self::drain_all(&mut state)
            } else {
                Vec::new()
            }
        };
        self.write_events(&flushed);

        let mut matched: u64 = 0;
        for (i, filter) in self.filters.iter().enumerate() {
            if <EnvFilter as tracing_subscriber::Layer<S>>::enabled(filter, meta, ctx.clone()) {
                matched |= 1 << i;
            }
        }
        if matched == 0 {
            return;
        }

        self.stats.received.fetch_add(1, Ordering::Relaxed);

        // Use the inner fmt layer to format the event into the thread-local capture buffer.
        <fmt::Layer<S, N, E, CaptureMakeWriter> as tracing_subscriber::Layer<S>>::on_event(
            &self.fmt_layer,
            event,
            ctx,
        );
        let bytes = take_captured(&self.fmt_layer.writer().0);
        if bytes.is_empty() {
            return;
        }

        let mut state = self.state.lock().unwrap();
        state.seq += 1;
        let mut current = (state.seq, bytes);
        for (i, reservoir) in state.reservoirs.iter_mut().enumerate() {
            if matched & (1 << i) == 0 {
                continue;
            }
            current = reservoir.sample(current);
            if current.1.is_empty() {
                self.stats.sampled.fetch_add(1, Ordering::Relaxed);
                return;
            }
        }
        self.stats.dropped.fetch_add(1, Ordering::Relaxed);
        return_captured(&self.fmt_layer.writer().0, current.1);
    }

    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        id: &tracing::span::Id,
        ctx: Context<'_, S>,
    ) {
        <fmt::Layer<S, N, E, CaptureMakeWriter> as tracing_subscriber::Layer<S>>::on_new_span(
            &self.fmt_layer,
            attrs,
            id,
            ctx,
        );
    }

    fn on_record(
        &self,
        id: &tracing::span::Id,
        values: &tracing::span::Record<'_>,
        ctx: Context<'_, S>,
    ) {
        <fmt::Layer<S, N, E, CaptureMakeWriter> as tracing_subscriber::Layer<S>>::on_record(
            &self.fmt_layer,
            id,
            values,
            ctx,
        );
    }

    fn on_enter(&self, id: &tracing::span::Id, ctx: Context<'_, S>) {
        <fmt::Layer<S, N, E, CaptureMakeWriter> as tracing_subscriber::Layer<S>>::on_enter(
            &self.fmt_layer,
            id,
            ctx,
        );
    }

    fn on_exit(&self, id: &tracing::span::Id, ctx: Context<'_, S>) {
        <fmt::Layer<S, N, E, CaptureMakeWriter> as tracing_subscriber::Layer<S>>::on_exit(
            &self.fmt_layer,
            id,
            ctx,
        );
    }

    fn on_close(&self, id: tracing::span::Id, ctx: Context<'_, S>) {
        <fmt::Layer<S, N, E, CaptureMakeWriter> as tracing_subscriber::Layer<S>>::on_close(
            &self.fmt_layer,
            id,
            ctx,
        );
    }
}
