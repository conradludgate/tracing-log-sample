use std::io::{self, Write};
use std::marker::PhantomData;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tracing::subscriber::Interest;
use tracing::{Event, Metadata, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::filter::EnvFilter;
use tracing_subscriber::fmt::format::{DefaultFields, Format, Full};
use tracing_subscriber::fmt::{self, FormatFields, MakeWriter};
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;

use crate::capture::{CaptureMakeWriter, return_captured, take_captured};
use crate::reservoir::Reservoir;

pub(crate) struct State {
    pub(crate) bucket_start: Instant,
    pub(crate) seq: u64,
    pub(crate) reservoirs: Vec<Reservoir<(u64, Vec<u8>)>>,
    pub(crate) pending: std::vec::IntoIter<(u64, Vec<u8>)>,
    pub(crate) last_release: Instant,
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
/// Sampled events are smeared across the bucket duration to reduce tail-latency
/// spikes from burst writes.
///
/// Construct via [`SamplingLayer::builder()`](crate::SamplingLayerBuilder).
pub struct SamplingLayer<
    S,
    N = DefaultFields,
    E = Format<Full>,
    W: for<'a> MakeWriter<'a> = fn() -> io::Stderr,
> {
    pub(crate) filters: Vec<EnvFilter>,
    pub(crate) state: Mutex<State>,
    pub(crate) bucket_duration: Duration,
    pub(crate) writer: W,
    pub(crate) fmt_layer: fmt::Layer<S, N, E, CaptureMakeWriter>,
    pub(crate) stats: Stats,
    pub(crate) _subscriber: PhantomData<fn(S)>,
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

    #[cold]
    fn write_events(&self, events: &[(u64, Vec<u8>)]) {
        if events.is_empty() {
            return;
        }
        let mut writer = self.writer.make_writer();
        for (_, buf) in events {
            let _ = writer.write_all(buf);
        }
    }

    fn smear_collect(
        state: &mut State,
        now: Instant,
        bucket_duration: Duration,
    ) -> Vec<(u64, Vec<u8>)> {
        let n = state.pending.len();
        if n == 0 {
            return Vec::new();
        }

        let bucket_end = state.bucket_start + bucket_duration;
        let remaining = bucket_end.saturating_duration_since(now);
        let to_release = if remaining.is_zero() {
            n
        } else {
            let interval = remaining / n as u32;
            if interval.is_zero() {
                n
            } else {
                let since_last = now.duration_since(state.last_release);
                (since_last.as_nanos() / interval.as_nanos()) as usize
            }
        };

        if to_release > 0 {
            let batch: Vec<_> = state.pending.by_ref().take(to_release).collect();
            state.last_release = now;
            batch
        } else {
            Vec::new()
        }
    }

    #[cold]
    fn rotate_bucket(&self, state: &mut State, batch: &mut Vec<(u64, Vec<u8>)>, now: Instant) {
        batch.extend(state.pending.by_ref());
        let drained = Self::drain_all(state);
        state.pending = drained.into_iter();
        state.bucket_start = now;
        state.last_release = now;
    }

    #[inline]
    fn tick_smear(&self) {
        let now = Instant::now();
        let to_write = {
            let mut state = self.state.lock().unwrap();
            let mut batch = Self::smear_collect(&mut state, now, self.bucket_duration);
            if now.duration_since(state.bucket_start) >= self.bucket_duration {
                self.rotate_bucket(&mut state, &mut batch, now);
            }
            batch
        };
        self.write_events(&to_write);
    }

    #[inline]
    fn match_filters<S2: Subscriber + for<'a> LookupSpan<'a>>(
        &self,
        meta: &Metadata<'_>,
        ctx: &Context<'_, S2>,
    ) -> u64 {
        let mut matched: u64 = 0;
        for (i, filter) in self.filters.iter().enumerate() {
            if <EnvFilter as tracing_subscriber::Layer<S2>>::enabled(filter, meta, ctx.clone()) {
                matched |= 1 << i;
            }
        }
        matched
    }

    #[cold]
    fn sample_event(&self, bytes: Vec<u8>, matched: u64) {
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

    /// Drain all reservoirs and write their contents immediately.
    pub fn flush(&self) {
        let (pending, drained) = {
            let mut state = self.state.lock().unwrap();
            let pending: Vec<_> = state.pending.by_ref().collect();
            let drained = Self::drain_all(&mut state);
            (pending, drained)
        };
        self.write_events(&pending);
        self.write_events(&drained);
    }
}

impl<S, N, E, W: for<'a> MakeWriter<'a>> Drop for SamplingLayer<S, N, E, W> {
    fn drop(&mut self) {
        if let Ok(mut state) = self.state.lock() {
            let pending: Vec<_> = state.pending.by_ref().collect();
            let drained = Self::drain_all(&mut state);
            drop(state);
            self.write_events(&pending);
            self.write_events(&drained);
        }
    }
}

type FmtLayer<S, N, E> = fmt::Layer<S, N, E, CaptureMakeWriter>;

impl<S, N, E, W> SamplingLayer<S, N, E, W>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'writer> FormatFields<'writer> + 'static,
    E: fmt::FormatEvent<S, N> + 'static,
    W: for<'a> MakeWriter<'a> + 'static,
{
    #[inline]
    fn inner(&self) -> &FmtLayer<S, N, E> {
        &self.fmt_layer
    }

    #[inline]
    fn format_event(&self, event: &Event<'_>, ctx: Context<'_, S>) -> Vec<u8> {
        self.inner().on_event(event, ctx);
        take_captured(&self.inner().writer().0)
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
        let matched = self.match_filters(event.metadata(), &ctx);
        if matched == 0 {
            return;
        }

        self.stats.received.fetch_add(1, Ordering::Relaxed);

        self.tick_smear();

        let bytes = self.format_event(event, ctx);
        if bytes.is_empty() {
            return;
        }

        self.sample_event(bytes, matched);
    }

    #[inline]
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        id: &tracing::span::Id,
        ctx: Context<'_, S>,
    ) {
        self.inner().on_new_span(attrs, id, ctx);
    }

    #[inline]
    fn on_record(
        &self,
        id: &tracing::span::Id,
        values: &tracing::span::Record<'_>,
        ctx: Context<'_, S>,
    ) {
        self.inner().on_record(id, values, ctx);
    }

    #[inline]
    fn on_enter(&self, id: &tracing::span::Id, ctx: Context<'_, S>) {
        self.inner().on_enter(id, ctx);
    }

    #[inline]
    fn on_exit(&self, id: &tracing::span::Id, ctx: Context<'_, S>) {
        self.inner().on_exit(id, ctx);
    }

    #[inline]
    fn on_close(&self, id: tracing::span::Id, ctx: Context<'_, S>) {
        self.inner().on_close(id, ctx);
    }
}
