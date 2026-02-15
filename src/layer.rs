use std::cell::Cell;
use std::io::{self, Write};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use thread_local::ThreadLocal;
use tracing::subscriber::Interest;
use tracing::{Event, Metadata, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::filter::EnvFilter;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::Context;

use crate::TextFormat;
use crate::format::FormatEvent;
use crate::reservoir::Reservoir;

pub(crate) struct State {
    pub(crate) bucket_index: u64,
    pub(crate) reservoirs: Vec<Reservoir>,
}

/// Counters tracking how many events were processed by the layer.
///
/// All counts are cumulative since layer creation.
pub struct Stats {
    /// Events that entered `on_event` (matched at least one filter).
    pub received: u64,
    /// Events that were kept in a reservoir (sampled).
    pub sampled: u64,
    /// Events that were dropped after failing to enter any reservoir.
    pub dropped: u64,
}

/// A [`tracing_subscriber::Layer`] that samples events into time-bucketed reservoirs.
///
/// Construct via [`SamplingLayer::builder()`](crate::SamplingLayerBuilder).
pub struct SamplingLayer<W: for<'a> MakeWriter<'a> = fn() -> io::Stderr, F = TextFormat> {
    pub(crate) filters: Vec<EnvFilter>,
    pub(crate) state: Mutex<State>,
    pub(crate) bucket_duration_ns: u64,
    pub(crate) writer: W,
    pub(crate) formatter: F,
    pub(crate) buf_cache: ThreadLocal<Cell<Vec<u8>>>,
    pub(crate) received: AtomicU64,
    pub(crate) sampled: AtomicU64,
    pub(crate) dropped: AtomicU64,
}

fn current_bucket_index(bucket_duration_ns: u64) -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64
        / bucket_duration_ns
}

impl<W: for<'a> MakeWriter<'a>, F: FormatEvent> SamplingLayer<W, F> {
    fn drain_all(state: &mut State) -> Vec<Vec<u8>> {
        state
            .reservoirs
            .iter_mut()
            .flat_map(|r| r.drain())
            .collect()
    }

    fn write_events(&self, events: &[Vec<u8>]) {
        if events.is_empty() {
            return;
        }
        let mut writer = self.writer.make_writer();
        for event in events {
            let _ = writer.write_all(event);
        }
    }

    /// Drain all reservoirs and write their contents immediately.
    pub fn flush(&self) {
        let events = Self::drain_all(&mut self.state.lock().unwrap());
        self.write_events(&events);
    }

    /// Return a snapshot of the layer's event counters.
    pub fn stats(&self) -> Stats {
        Stats {
            received: self.received.load(Ordering::Relaxed),
            sampled: self.sampled.load(Ordering::Relaxed),
            dropped: self.dropped.load(Ordering::Relaxed),
        }
    }
}

impl<W: for<'a> MakeWriter<'a>, F> Drop for SamplingLayer<W, F> {
    fn drop(&mut self) {
        if let Ok(mut state) = self.state.lock() {
            let events: Vec<Vec<u8>> = state
                .reservoirs
                .iter_mut()
                .flat_map(|r| r.drain())
                .collect();
            let mut writer = self.writer.make_writer();
            for event in &events {
                let _ = writer.write_all(event);
            }
        }
    }
}

impl<S, W, F> Layer<S> for SamplingLayer<W, F>
where
    S: Subscriber,
    W: for<'a> MakeWriter<'a> + 'static,
    F: FormatEvent + 'static,
{
    fn register_callsite(&self, meta: &'static Metadata<'static>) -> Interest {
        for filter in &self.filters {
            let interest = <EnvFilter as Layer<S>>::register_callsite(filter, meta);
            if interest.is_sometimes() || interest.is_always() {
                return Interest::sometimes();
            }
        }
        Interest::never()
    }

    fn enabled(&self, meta: &Metadata<'_>, ctx: Context<'_, S>) -> bool {
        self.filters
            .iter()
            .any(|filter| <EnvFilter as Layer<S>>::enabled(filter, meta, ctx.clone()))
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
            if <EnvFilter as Layer<S>>::enabled(filter, meta, ctx.clone()) {
                matched |= 1 << i;
            }
        }
        if matched == 0 {
            return;
        }

        self.received.fetch_add(1, Ordering::Relaxed);

        let cache = self.buf_cache.get_or_default();
        let mut current = cache.take();
        current.clear();
        if self.formatter.format_event(event, &mut current).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            cache.set(current);
            return;
        }

        let mut state = self.state.lock().unwrap();
        for (i, reservoir) in state.reservoirs.iter_mut().enumerate() {
            if matched & (1 << i) == 0 {
                continue;
            }
            current = reservoir.sample(current);
            if current.is_empty() {
                self.sampled.fetch_add(1, Ordering::Relaxed);
                cache.set(current);
                return;
            }
        }
        self.dropped.fetch_add(1, Ordering::Relaxed);
        current.clear();
        cache.set(current);
    }
}
