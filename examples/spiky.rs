use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tracing_subscriber::Registry;
use tracing_subscriber::filter::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

fn main() {
    let sampled = std::env::args().any(|a| a == "--sampled");

    if sampled {
        let (layer, _stats) = tracing_log_sample::SamplingLayer::<Registry>::builder()
            .bucket_duration(Duration::from_millis(500))
            .budget(EnvFilter::new("error"), 20)
            .budget(EnvFilter::new("warn"), 10)
            .budget(EnvFilter::new("info"), 6)
            .budget(EnvFilter::new("trace"), 4)
            .build();
        Registry::default().with(layer).init();
    } else {
        Registry::default()
            .with(tracing_subscriber::fmt::layer())
            .init();

    }

    run();
}

fn tick_rng(tick: u64) -> fastrand::Rng {
    fastrand::Rng::with_seed(tick)
}

fn run() {
    let duration = Duration::from_secs(10);
    let tick_interval = Duration::from_millis(10);

    let start_tick = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
        / tick_interval.as_millis() as u64;

    for t in 0.. {
        let tick = start_tick + t;
        let mut rng = tick_rng(tick);

        let elapsed = Instant::now();

        let is_spike = rng.f64() < 0.01;
        let burst = if is_spike { rng.usize(50..150) } else { rng.usize(1..5) };

        for i in 0..burst {
            let latency_ms = if is_spike {
                rng.f64() * 500.0
            } else {
                rng.f64() * 20.0
            };

            if latency_ms > 400.0 {
                tracing::error!(latency_ms, i, "very slow request");
            } else if latency_ms > 100.0 {
                tracing::warn!(latency_ms, i, "slow request");
            } else if latency_ms > 50.0 {
                tracing::info!(latency_ms, i, "moderate request");
            } else {
                tracing::debug!(latency_ms, i, "normal request");
            }
        }

        if elapsed.elapsed() >= duration {
            break;
        }

        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let target_ms = (start_tick + t + 1) * tick_interval.as_millis() as u64;
        if let Some(wait) = target_ms.checked_sub(now_ms) {
            std::thread::sleep(Duration::from_millis(wait));
        }
    }
}
