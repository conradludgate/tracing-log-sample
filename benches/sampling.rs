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
        std::thread::sleep(Duration::from_micros(100));
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
    let mut builder = SamplingLayer::builder()
        .bucket_duration(Duration::from_millis(50))
        .writer(SlowWriter);
    for &(filter, limit) in budgets {
        builder = builder.budget(EnvFilter::new(filter), limit);
    }
    let (layer, _stats) = builder.build();
    Dispatch::new(Registry::default().with(layer))
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

    let dispatch = sampling_layer(&[("error", 1000)]);
    group.bench_function("matching", |b| {
        bench_threaded(&dispatch, 1, b, emit_error);
    });

    let dispatch = sampling_layer(&[("error", 1000)]);
    group.bench_function("non_matching", |b| {
        bench_threaded(&dispatch, 1, b, emit_info);
    });

    let dispatch = sampling_layer(&[("[active_span]=error", 1000)]);
    group.bench_function("dynamic_non_matching", |b| {
        bench_threaded(&dispatch, 1, b, emit_error);
    });

    let dispatch = sampling_layer(&[("error", 100), ("trace", 1000)]);
    group.bench_function("multi_budget", |b| {
        bench_threaded(&dispatch, 1, b, emit_error);
    });

    group.finish();
}

fn bench_contention(c: &mut Criterion) {
    let mut group = c.benchmark_group("contention");
    for threads in [1, 2, 4, 8] {
        let dispatch = sampling_layer(&[("error", 1000)]);
        group.bench_with_input(
            BenchmarkId::new("matching", threads),
            &threads,
            |b, &threads| bench_threaded(&dispatch, threads, b, emit_error),
        );
    }
    group.finish();
}

criterion_group!(benches, bench_single_thread, bench_contention);
criterion_main!(benches);
