//! A [`tracing_subscriber::Layer`] that rate-limits log output using reservoir sampling.
//!
//! Events are collected into fixed-duration time buckets and sampled using
//! Algorithm R, producing a statistically uniform sample per bucket. Multiple
//! sampling budgets can be configured with [`EnvFilter`](tracing_subscriber::filter::EnvFilter)
//! patterns — events displaced from one budget's reservoir cascade to the next
//! matching budget.
//!
//! Formatting is delegated to [`tracing_subscriber::fmt::Layer`], so all the
//! usual formatting options (compact, pretty, JSON, timestamps, etc.) work
//! out of the box.
//!
//! # Example
//!
//! ```
//! use std::time::Duration;
//! use tracing_subscriber::{Registry, filter::EnvFilter, layer::SubscriberExt};
//! use tracing_log_sample::SamplingLayer;
//!
//! let (layer, stats) = SamplingLayer::<Registry>::builder()
//!     .bucket_duration(Duration::from_millis(50))
//!     .budget(EnvFilter::new("error"), 1000)
//!     .budget(EnvFilter::new("info"), 5000)
//!     .build();
//!
//! let subscriber = Registry::default().with(layer);
//! // stats.received(), stats.sampled(), stats.dropped()
//! // tracing::subscriber::set_global_default(subscriber).unwrap();
//! ```

mod builder;
mod capture;
mod layer;
mod reservoir;

pub use builder::SamplingLayerBuilder;
pub use layer::{SamplingLayer, Stats};

#[cfg(test)]
mod tests {
    use std::io::{self, Write};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use tracing_subscriber::Registry;
    use tracing_subscriber::filter::EnvFilter;
    use tracing_subscriber::fmt::MakeWriter;
    use tracing_subscriber::layer::SubscriberExt;

    use crate::SamplingLayer;

    #[derive(Clone, Default)]
    struct SharedBuf(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().write(buf)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for SharedBuf {
        type Writer = SharedBuf;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    impl SharedBuf {
        fn lines(&self) -> Vec<String> {
            let raw = self.0.lock().unwrap();
            let s = String::from_utf8_lossy(&raw);
            s.lines().map(String::from).collect()
        }
    }

    fn capture_layer(
        bucket_ms: u64,
        budgets: &[(&str, u64)],
    ) -> (impl tracing_subscriber::Layer<Registry>, SharedBuf) {
        let buf = SharedBuf::default();
        let mut builder = SamplingLayer::<Registry>::builder()
            .without_time()
            .with_target(false)
            .bucket_duration(Duration::from_millis(bucket_ms))
            .writer(buf.clone());
        for &(filter, limit) in budgets {
            builder = builder.budget(EnvFilter::new(filter), limit);
        }
        let (layer, _stats) = builder.build();
        (layer, buf)
    }

    #[test]
    fn reservoir_keeps_at_most_limit() {
        let (layer, buf) = capture_layer(1_000, &[("error", 10)]);
        let subscriber = Registry::default().with(layer);

        tracing::subscriber::with_default(subscriber, || {
            for _ in 0..100 {
                tracing::error!("event");
            }
        });

        let lines = buf.lines();
        assert_eq!(lines.len(), 10, "expected 10 events, got {}", lines.len());
    }

    #[test]
    fn ejected_events_cascade_to_next_budget() {
        let (layer, buf) = capture_layer(1_000, &[("error", 5), ("trace", 50)]);
        let subscriber = Registry::default().with(layer);

        tracing::subscriber::with_default(subscriber, || {
            for _ in 0..100 {
                tracing::error!("event");
            }
        });

        let lines = buf.lines();
        assert!(
            lines.len() > 5,
            "cascade should produce more than budget 1's limit of 5, got {}",
            lines.len()
        );
        assert!(
            lines.len() <= 55,
            "total should be at most 5 + 50 = 55, got {}",
            lines.len()
        );
    }

    #[test]
    fn non_matching_events_are_dropped() {
        let (layer, buf) = capture_layer(1_000, &[("error", 100)]);
        let subscriber = Registry::default().with(layer);

        tracing::subscriber::with_default(subscriber, || {
            for _ in 0..50 {
                tracing::debug!("should be dropped");
            }
        });

        let lines = buf.lines();
        assert_eq!(lines.len(), 0, "debug events should not match error filter");
    }

    #[test]
    fn multiple_budgets_separate_levels() {
        let (layer, buf) = capture_layer(1_000, &[("error", 10), ("debug", 10)]);
        let subscriber = Registry::default().with(layer);

        tracing::subscriber::with_default(subscriber, || {
            for _ in 0..50 {
                tracing::error!("err");
            }
            for _ in 0..50 {
                tracing::debug!("dbg");
            }
        });

        let lines = buf.lines();
        let error_count = lines.iter().filter(|l| l.contains("ERROR")).count();
        let debug_count = lines.iter().filter(|l| l.contains("DEBUG")).count();

        assert!(
            error_count >= 10,
            "should have at least 10 errors, got {error_count}"
        );
        assert!(
            debug_count >= 1,
            "should have at least 1 debug, got {debug_count}"
        );
    }

    #[test]
    fn flushed_events_are_in_arrival_order() {
        let (layer, buf) = capture_layer(1_000, &[("error", 10)]);
        let subscriber = Registry::default().with(layer);

        tracing::subscriber::with_default(subscriber, || {
            for i in 0..200 {
                tracing::error!(i, "seq");
            }
        });

        let lines = buf.lines();
        assert_eq!(lines.len(), 10);

        let numbers: Vec<usize> = lines
            .iter()
            .map(|line| {
                let s = line.rsplit("i=").next().unwrap().trim();
                s.parse().unwrap()
            })
            .collect();

        for w in numbers.windows(2) {
            assert!(
                w[0] < w[1],
                "events not in arrival order: {} came before {}",
                w[0],
                w[1]
            );
        }
    }

    #[test]
    fn flushed_cascade_events_are_in_arrival_order() {
        let (layer, buf) = capture_layer(1_000, &[("error", 5), ("trace", 10)]);
        let subscriber = Registry::default().with(layer);

        tracing::subscriber::with_default(subscriber, || {
            for i in 0..200u32 {
                if i % 2 == 0 {
                    tracing::error!(i, "seq");
                } else {
                    tracing::trace!(i, "seq");
                }
            }
        });

        let lines = buf.lines();
        assert!(
            lines.len() > 5,
            "cascade should produce more than first budget's limit of 5, got {}",
            lines.len()
        );

        let numbers: Vec<usize> = lines
            .iter()
            .map(|line| {
                let s = line.rsplit("i=").next().unwrap().trim();
                s.parse().unwrap()
            })
            .collect();

        for w in numbers.windows(2) {
            assert!(
                w[0] < w[1],
                "events not in arrival order: {} came before {}",
                w[0],
                w[1]
            );
        }
    }

    #[test]
    fn bucket_rotation_flushes() {
        let (layer, buf) = capture_layer(50, &[("trace", 1000)]);
        let subscriber = Registry::default().with(layer);

        tracing::subscriber::with_default(subscriber, || {
            for _ in 0..10 {
                tracing::info!("batch1");
            }
            std::thread::sleep(Duration::from_millis(60));
            tracing::info!("batch2");
        });

        let lines = buf.lines();
        assert!(
            lines.len() >= 10,
            "should have flushed batch1 on rotation, got {}",
            lines.len()
        );
    }
}
