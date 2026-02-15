use std::io::{self, Write};
use std::sync::Mutex;
use std::time::Duration;

use tracing_subscriber::filter::EnvFilter;

use crate::TextFormat;
use crate::format::FormatEvent;
use thread_local::ThreadLocal;

use crate::layer::{SamplingLayer, State};
use crate::reservoir::Reservoir;

pub struct SamplingLayerBuilder<F = TextFormat> {
    phases: Vec<(EnvFilter, u64)>,
    bucket_duration: Duration,
    writer: Box<dyn Write + Send>,
    formatter: F,
}

impl SamplingLayer {
    pub fn builder() -> SamplingLayerBuilder {
        SamplingLayerBuilder {
            phases: Vec::new(),
            bucket_duration: Duration::from_millis(50),
            writer: Box::new(io::stderr()),
            formatter: TextFormat,
        }
    }
}

impl<F> SamplingLayerBuilder<F> {
    pub fn phase(mut self, filter: &str, limit_per_second: u64) -> Self {
        self.phases.push((EnvFilter::new(filter), limit_per_second));
        self
    }

    pub fn bucket_duration(mut self, duration: Duration) -> Self {
        self.bucket_duration = duration;
        self
    }

    pub fn writer<W: Write + Send + 'static>(mut self, writer: W) -> Self {
        self.writer = Box::new(writer);
        self
    }

    pub fn formatter<F2>(self, formatter: F2) -> SamplingLayerBuilder<F2> {
        SamplingLayerBuilder {
            phases: self.phases,
            bucket_duration: self.bucket_duration,
            writer: self.writer,
            formatter,
        }
    }
}

impl<F: FormatEvent> SamplingLayerBuilder<F> {
    pub fn build(self) -> SamplingLayer<F> {
        let bucket_ms = self.bucket_duration.as_millis() as u64;
        assert!(bucket_ms > 0, "bucket_duration must be > 0");

        let mut filters = Vec::new();
        let mut reservoirs = Vec::new();
        for (filter, limit_per_second) in self.phases {
            let limit_per_bucket =
                ((limit_per_second as f64 * bucket_ms as f64 / 1000.0).ceil() as usize).max(1);
            filters.push(filter);
            reservoirs.push(Reservoir::new(limit_per_bucket));
        }

        SamplingLayer {
            filters,
            state: Mutex::new(State {
                bucket_index: 0,
                reservoirs,
            }),
            bucket_duration_ms: bucket_ms,
            writer: Mutex::new(self.writer),
            formatter: self.formatter,
            buf_cache: ThreadLocal::new(),
        }
    }
}
