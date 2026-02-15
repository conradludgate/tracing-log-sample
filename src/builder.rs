use std::io;
use std::sync::Mutex;
use std::time::Duration;

use thread_local::ThreadLocal;
use tracing_subscriber::filter::EnvFilter;
use tracing_subscriber::fmt::MakeWriter;

use crate::TextFormat;
use crate::format::FormatEvent;
use crate::layer::{SamplingLayer, State};
use crate::reservoir::Reservoir;

/// Builder for [`SamplingLayer`](crate::SamplingLayer).
///
/// Created via [`SamplingLayer::builder()`](crate::SamplingLayer::builder).
pub struct SamplingLayerBuilder<W = fn() -> io::Stderr, F = TextFormat> {
    phases: Vec<(EnvFilter, u64)>,
    bucket_duration: Duration,
    writer: W,
    formatter: F,
}

impl SamplingLayer {
    pub fn builder() -> SamplingLayerBuilder {
        SamplingLayerBuilder {
            phases: Vec::new(),
            bucket_duration: Duration::from_millis(50),
            writer: io::stderr as fn() -> io::Stderr,
            formatter: TextFormat,
        }
    }
}

impl<W, F> SamplingLayerBuilder<W, F> {
    /// Add a sampling phase with an [`EnvFilter`] pattern and a per-second event budget.
    pub fn phase(mut self, filter: &str, limit_per_second: u64) -> Self {
        self.phases.push((EnvFilter::new(filter), limit_per_second));
        self
    }

    /// Set the time bucket duration. Defaults to 50ms.
    pub fn bucket_duration(mut self, duration: Duration) -> Self {
        self.bucket_duration = duration;
        self
    }

    /// Set the output writer. Defaults to stderr.
    pub fn writer<W2>(self, writer: W2) -> SamplingLayerBuilder<W2, F> {
        SamplingLayerBuilder {
            phases: self.phases,
            bucket_duration: self.bucket_duration,
            writer,
            formatter: self.formatter,
        }
    }

    /// Replace the event formatter.
    pub fn formatter<F2>(self, formatter: F2) -> SamplingLayerBuilder<W, F2> {
        SamplingLayerBuilder {
            phases: self.phases,
            bucket_duration: self.bucket_duration,
            writer: self.writer,
            formatter,
        }
    }
}

impl<W: for<'a> MakeWriter<'a> + 'static, F: FormatEvent> SamplingLayerBuilder<W, F> {
    /// Consume the builder and create a [`SamplingLayer`](crate::SamplingLayer).
    pub fn build(self) -> SamplingLayer<W, F> {
        let bucket_ns = self.bucket_duration.as_nanos() as u64;
        assert!(bucket_ns > 0, "bucket_duration must be > 0");

        let bucket_secs = self.bucket_duration.as_secs_f64();
        let mut filters = Vec::new();
        let mut reservoirs = Vec::new();
        for (filter, limit_per_second) in self.phases {
            let limit_per_bucket = ((limit_per_second as f64 * bucket_secs).ceil() as usize).max(1);
            filters.push(filter);
            reservoirs.push(Reservoir::new(limit_per_bucket));
        }

        SamplingLayer {
            filters,
            state: Mutex::new(State {
                bucket_index: 0,
                reservoirs,
            }),
            bucket_duration_ns: bucket_ns,
            writer: self.writer,
            formatter: self.formatter,
            buf_cache: ThreadLocal::new(),
        }
    }
}
