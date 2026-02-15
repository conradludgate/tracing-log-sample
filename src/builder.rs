use std::io;
use std::marker::PhantomData;
use std::sync::Mutex;
use std::time::Duration;

use tracing::Subscriber;
use tracing_subscriber::filter::EnvFilter;
use tracing_subscriber::fmt::format::{DefaultFields, Format, Full};
use tracing_subscriber::fmt::{self, FormatFields, MakeWriter};
use tracing_subscriber::registry::LookupSpan;

use crate::capture::CaptureMakeWriter;
use crate::layer::{SamplingLayer, State, Stats};
use crate::reservoir::Reservoir;

/// Builder for [`SamplingLayer`](crate::SamplingLayer).
///
/// Created via [`SamplingLayer::builder()`](crate::SamplingLayer::builder).
pub struct SamplingLayerBuilder<S, N = DefaultFields, E = Format<Full>, W = fn() -> io::Stderr> {
    budgets: Vec<(EnvFilter, u64)>,
    bucket_duration: Duration,
    writer: W,
    fmt_layer: fmt::Layer<S, N, E, CaptureMakeWriter>,
    _subscriber: PhantomData<fn(S)>,
}

impl<S> SamplingLayer<S> {
    pub fn builder() -> SamplingLayerBuilder<S> {
        SamplingLayerBuilder {
            budgets: Vec::new(),
            bucket_duration: Duration::from_millis(50),
            writer: io::stderr as fn() -> io::Stderr,
            fmt_layer: fmt::Layer::default().with_writer(CaptureMakeWriter::default()),
            _subscriber: PhantomData,
        }
    }
}

impl<S, N, E, W> SamplingLayerBuilder<S, N, E, W> {
    /// Add a sampling budget with an [`EnvFilter`] and a per-second event limit.
    ///
    /// Budgets whose limit rounds to zero events per bucket are skipped.
    pub fn budget(mut self, filter: EnvFilter, limit_per_second: u64) -> Self {
        self.budgets.push((filter, limit_per_second));
        self
    }

    /// Set the time bucket duration. Defaults to 50ms.
    pub fn bucket_duration(mut self, duration: Duration) -> Self {
        self.bucket_duration = duration;
        self
    }

    /// Set the output writer. Defaults to stderr.
    pub fn writer<W2>(self, writer: W2) -> SamplingLayerBuilder<S, N, E, W2> {
        SamplingLayerBuilder {
            budgets: self.budgets,
            bucket_duration: self.bucket_duration,
            writer,
            fmt_layer: self.fmt_layer,
            _subscriber: PhantomData,
        }
    }
}

impl<S, N, E, W> SamplingLayerBuilder<S, N, E, W>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'writer> FormatFields<'writer> + 'static,
    E: fmt::FormatEvent<S, N> + 'static,
{
    /// Sets the event formatter.
    pub fn event_format<E2>(self, e: E2) -> SamplingLayerBuilder<S, N, E2, W>
    where
        E2: fmt::FormatEvent<S, N> + 'static,
    {
        SamplingLayerBuilder {
            budgets: self.budgets,
            bucket_duration: self.bucket_duration,
            writer: self.writer,
            fmt_layer: self.fmt_layer.event_format(e),
            _subscriber: PhantomData,
        }
    }

    /// Updates the event formatter by applying a function to the existing one.
    pub fn map_event_format<E2>(self, f: impl FnOnce(E) -> E2) -> SamplingLayerBuilder<S, N, E2, W>
    where
        E2: fmt::FormatEvent<S, N> + 'static,
    {
        SamplingLayerBuilder {
            budgets: self.budgets,
            bucket_duration: self.bucket_duration,
            writer: self.writer,
            fmt_layer: self.fmt_layer.map_event_format(f),
            _subscriber: PhantomData,
        }
    }

    /// Sets the field formatter.
    pub fn fmt_fields<N2>(self, fmt_fields: N2) -> SamplingLayerBuilder<S, N2, E, W>
    where
        N2: for<'writer> FormatFields<'writer> + 'static,
    {
        SamplingLayerBuilder {
            budgets: self.budgets,
            bucket_duration: self.bucket_duration,
            writer: self.writer,
            fmt_layer: self.fmt_layer.fmt_fields(fmt_fields),
            _subscriber: PhantomData,
        }
    }
}

impl<S, N, L, T, W> SamplingLayerBuilder<S, N, Format<L, T>, W>
where
    N: for<'writer> FormatFields<'writer> + 'static,
{
    /// Do not emit timestamps.
    pub fn without_time(self) -> SamplingLayerBuilder<S, N, Format<L, ()>, W> {
        SamplingLayerBuilder {
            budgets: self.budgets,
            bucket_duration: self.bucket_duration,
            writer: self.writer,
            fmt_layer: self.fmt_layer.without_time(),
            _subscriber: PhantomData,
        }
    }

    /// Sets whether or not an event's target is displayed.
    pub fn with_target(self, display_target: bool) -> Self {
        SamplingLayerBuilder {
            fmt_layer: self.fmt_layer.with_target(display_target),
            ..self
        }
    }

    /// Sets whether or not an event's level is displayed.
    pub fn with_level(self, display_level: bool) -> Self {
        SamplingLayerBuilder {
            fmt_layer: self.fmt_layer.with_level(display_level),
            ..self
        }
    }

    /// Use the compact formatter.
    pub fn compact(
        self,
    ) -> SamplingLayerBuilder<S, N, Format<tracing_subscriber::fmt::format::Compact, T>, W>
    where
        N: for<'writer> FormatFields<'writer> + 'static,
    {
        SamplingLayerBuilder {
            budgets: self.budgets,
            bucket_duration: self.bucket_duration,
            writer: self.writer,
            fmt_layer: self.fmt_layer.compact(),
            _subscriber: PhantomData,
        }
    }
}

impl<S, N, E, W> SamplingLayerBuilder<S, N, E, W>
where
    W: for<'a> MakeWriter<'a> + 'static,
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'writer> FormatFields<'writer> + 'static,
    E: fmt::FormatEvent<S, N> + 'static,
{
    /// Consume the builder and create a [`SamplingLayer`](crate::SamplingLayer)
    /// and a [`Stats`] handle for reading event counters.
    pub fn build(self) -> (SamplingLayer<S, N, E, W>, Stats) {
        let bucket_ns = self.bucket_duration.as_nanos() as u64;
        assert!(bucket_ns > 0, "bucket_duration must be > 0");

        let bucket_secs = self.bucket_duration.as_secs_f64();
        let mut filters = Vec::new();
        let mut reservoirs = Vec::new();
        for (filter, limit_per_second) in self.budgets {
            let limit_per_bucket = (limit_per_second as f64 * bucket_secs).ceil() as usize;
            if limit_per_bucket == 0 {
                continue;
            }
            filters.push(filter);
            reservoirs.push(Reservoir::new(limit_per_bucket));
        }

        let stats = Stats::new();
        let layer = SamplingLayer {
            filters,
            state: Mutex::new(State {
                bucket_index: 0,
                reservoirs,
            }),
            bucket_duration_ns: bucket_ns,
            writer: self.writer,
            fmt_layer: self.fmt_layer,
            stats: stats.clone(),
            _subscriber: PhantomData,
        };
        (layer, stats)
    }
}
