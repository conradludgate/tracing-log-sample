//! A [`tracing_subscriber::Layer`] that rate-limits log output using reservoir sampling.
//!
//! Events are collected into fixed-duration time buckets and sampled using
//! Algorithm R, producing a statistically uniform sample per bucket. Multiple
//! sampling phases can be configured with [`EnvFilter`](tracing_subscriber::filter::EnvFilter)
//! patterns — events displaced from one phase's reservoir cascade to the next
//! matching phase.
//!
//! # Example
//!
//! ```
//! use std::time::Duration;
//! use tracing_subscriber::{Registry, layer::SubscriberExt};
//! use tracing_log_sample::SamplingLayer;
//!
//! let layer = SamplingLayer::builder()
//!     .bucket_duration(Duration::from_millis(50))
//!     .phase("error", 1000)
//!     .phase("info", 5000)
//!     .build();
//!
//! let subscriber = Registry::default().with(layer);
//! // tracing::subscriber::set_global_default(subscriber).unwrap();
//! ```

mod builder;
mod format;
mod layer;
mod reservoir;

pub use builder::SamplingLayerBuilder;
pub use format::{FormatEvent, TextFormat};
pub use layer::SamplingLayer;

#[cfg(test)]
mod tests {
    use std::io::{self, Write};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use tracing_subscriber::Registry;
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
        phases: &[(&str, u64)],
    ) -> (SamplingLayer<SharedBuf>, SharedBuf) {
        let buf = SharedBuf::default();
        let mut builder = SamplingLayer::builder()
            .bucket_duration(Duration::from_millis(bucket_ms))
            .writer(buf.clone());
        for &(filter, limit) in phases {
            builder = builder.phase(filter, limit);
        }
        (builder.build(), buf)
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
    fn ejected_events_cascade_to_next_phase() {
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
            "cascade should produce more than phase 1's limit of 5, got {}",
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
    fn multiple_phases_separate_levels() {
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
