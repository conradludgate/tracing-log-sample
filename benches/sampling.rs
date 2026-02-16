use std::hint::black_box;
use std::io::{self, Write};
use std::sync::Barrier;
use std::time::Duration;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use tracing::dispatcher::{self, Dispatch};
use tracing_log_sample::SamplingLayer;
use tracing_subscriber::Registry;
use tracing_subscriber::filter::EnvFilter;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;

struct SlowWriter;

impl Write for SlowWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        std::thread::sleep(Duration::from_nanos(1500) * buf.len() as u32);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for SlowWriter {
    type Writer = SlowWriter;
    fn make_writer(&'a self) -> Self::Writer {
        SlowWriter
    }
}

fn sampling_layer(budgets: &[(&str, u64)]) -> Dispatch {
    let mut builder = SamplingLayer::<Registry>::builder()
        .without_time()
        .bucket_duration(Duration::from_micros(500))
        .writer(SlowWriter);
    for &(filter, limit) in budgets {
        builder = builder.budget(EnvFilter::new(filter), limit);
    }
    let (layer, _stats) = builder.build();
    Dispatch::new(Registry::default().with(layer))
}

fn baseline_layer() -> Dispatch {
    let layer = tracing_subscriber::fmt::layer()
        .without_time()
        .with_writer(SlowWriter);
    Dispatch::new(Registry::default().with(layer))
}

fn warm_dispatch(dispatch: &Dispatch, events: usize) {
    dispatcher::with_default(dispatch, || {
        for _ in 0..events {
            tracing::error!(i = black_box(42), "warmup");
        }
    });
    std::thread::sleep(Duration::from_millis(2));
    dispatcher::with_default(dispatch, || {
        tracing::error!("rotate");
    });
}

fn bench_threaded(
    dispatch: &Dispatch,
    threads: usize,
    b: &mut criterion::Bencher,
    f: impl Fn() + Sync,
) {
    b.iter_custom(|iters| {
        let barrier = Barrier::new(threads);
        let per_thread = iters / threads as u64;
        std::thread::scope(|s| {
            let handles: Vec<_> = (0..threads)
                .map(|_| {
                    s.spawn(|| {
                        dispatcher::with_default(dispatch, || {
                            barrier.wait();
                            let start = std::time::Instant::now();
                            for _ in 0..per_thread {
                                f();
                            }
                            start.elapsed()
                        })
                    })
                })
                .collect();
            let total: Duration = handles.into_iter().map(|h| h.join().unwrap()).sum();
            total / threads as u32
        })
    });
}

fn emit_error() {
    tracing::error!(i = black_box(42), "benchmark event");
}

fn emit_info() {
    tracing::info!(i = black_box(42), "benchmark event");
}

fn bench_single_thread(c: &mut Criterion) {
    let mut group = c.benchmark_group("single");

    let dispatch = sampling_layer(&[("error", 100_000)]);
    group.bench_function("matching", |b| {
        bench_threaded(&dispatch, 1, b, emit_error);
    });

    let dispatch = sampling_layer(&[("error", 100_000)]);
    group.bench_function("non_matching", |b| {
        bench_threaded(&dispatch, 1, b, emit_info);
    });

    let dispatch = sampling_layer(&[("[active_span]=error", 100_000)]);
    group.bench_function("dynamic_non_matching", |b| {
        bench_threaded(&dispatch, 1, b, emit_error);
    });

    let dispatch = sampling_layer(&[("error", 10_000), ("trace", 100_000)]);
    group.bench_function("multi_budget", |b| {
        bench_threaded(&dispatch, 1, b, emit_error);
    });

    group.finish();
}

fn bench_contention(c: &mut Criterion) {
    let mut group = c.benchmark_group("contention");
    for threads in [1, 2, 4, 8] {
        let dispatch = sampling_layer(&[("error", 1_000_000)]);
        group.bench_with_input(
            BenchmarkId::new("matching", threads),
            &threads,
            |b, &threads| bench_threaded(&dispatch, threads, b, emit_error),
        );
    }
    group.finish();
}

fn bench_vs_baseline(c: &mut Criterion) {
    let mut group = c.benchmark_group("vs_baseline");

    // 8 threads × ~52k logs/s each ≈ 421k logs/s ≈ 210 logs per 500µs bucket.
    let scenarios: &[(&str, &[(&str, u64)])] = &[
        // 1_000_000/s * 0.0005s = 500 per bucket. Well above ~210 throughput.
        ("below_threshold", &[("error", 1_000_000)]),
        // 400_000/s * 0.0005s = 200 per bucket. Right at ~210 throughput.
        ("above_threshold", &[("error", 400_000)]),
        // 100_000/s * 0.0005s = 50 per bucket. ~4× oversubscribed.
        ("heavy_load", &[("error", 100_000)]),
    ];

    for &(scenario, budgets) in scenarios {
        let dispatch = sampling_layer(budgets);
        warm_dispatch(&dispatch, 100);
        group.bench_function(BenchmarkId::new(scenario, "sampled"), |b| {
            bench_threaded(&dispatch, 8, b, emit_error)
        });

        let dispatch = baseline_layer();
        group.bench_function(BenchmarkId::new(scenario, "baseline"), |b| {
            bench_threaded(&dispatch, 8, b, emit_error)
        });
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_single_thread,
    bench_contention,
    bench_vs_baseline
);
criterion_main!(benches);
