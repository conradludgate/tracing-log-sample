use std::io::Write;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use tracing::subscriber::Interest;
use tracing::{Event, Metadata, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::filter::EnvFilter;
use tracing_subscriber::layer::Context;

use crate::TextFormat;
use crate::format::FormatEvent;
use crate::reservoir::Reservoir;

pub(crate) struct State {
    pub(crate) bucket_index: u64,
    pub(crate) reservoirs: Vec<Reservoir>,
}

pub struct SamplingLayer<F = TextFormat> {
    pub(crate) filters: Vec<EnvFilter>,
    pub(crate) state: Mutex<State>,
    pub(crate) bucket_duration_ms: u64,
    pub(crate) writer: Mutex<Box<dyn Write + Send>>,
    pub(crate) formatter: F,
}

fn current_bucket_index(bucket_duration_ms: u64) -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
        / bucket_duration_ms
}

impl<F: FormatEvent> SamplingLayer<F> {
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
        let mut writer = self.writer.lock().unwrap();
        for event in events {
            let _ = writer.write_all(event);
        }
    }

    pub fn flush(&self) {
        let events = Self::drain_all(&mut self.state.lock().unwrap());
        self.write_events(&events);
    }
}

impl<F> Drop for SamplingLayer<F> {
    fn drop(&mut self) {
        if let Ok(mut state) = self.state.lock() {
            let events: Vec<Vec<u8>> = state
                .reservoirs
                .iter_mut()
                .flat_map(|r| r.drain())
                .collect();
            if let Ok(mut writer) = self.writer.lock() {
                for event in &events {
                    let _ = writer.write_all(event);
                }
            }
        }
    }
}

impl<S, F> Layer<S> for SamplingLayer<F>
where
    S: Subscriber,
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
        let bucket_index = current_bucket_index(self.bucket_duration_ms);

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

        let mut current = Vec::new();
        if self.formatter.format_event(event, &mut current).is_err() {
            return;
        }

        let mut state = self.state.lock().unwrap();
        for (i, reservoir) in state.reservoirs.iter_mut().enumerate() {
            if matched & (1 << i) == 0 {
                continue;
            }
            current = reservoir.sample(current);
            if current.is_empty() {
                return;
            }
        }
    }
}
