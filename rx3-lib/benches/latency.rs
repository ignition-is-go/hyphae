use criterion::{black_box, criterion_group, criterion_main, Criterion, BenchmarkId};
use rx3::{Cell, FilterExt, MapExt, Mutable, ScanExt, Watchable};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

fn bench_single_cell_propagation(c: &mut Criterion) {
    c.bench_function("single cell set + watch", |b| {
        let cell = Cell::new(0u64);
        let counter = Arc::new(AtomicU64::new(0));
        let cnt = counter.clone();
        cell.watch(move |_| {
            cnt.fetch_add(1, Ordering::Relaxed);
        });

        b.iter(|| {
            cell.set(black_box(42));
        });
    });
}

fn bench_chain_depth(c: &mut Criterion) {
    let mut group = c.benchmark_group("chain depth propagation");

    for depth in [1, 5, 10, 20, 50, 100].iter() {
        group.bench_with_input(BenchmarkId::new("map chain", depth), depth, |b, &depth| {
            let source = Cell::new(0u64);

            let mut current: Box<dyn Fn() -> u64 + Send + Sync> = {
                let s = source.clone();
                Box::new(move || s.get())
            };

            // Build chain of maps
            let mut last = source.map(|x| x + 1);
            for _ in 1..depth {
                last = last.map(|x| x + 1);
            }

            let counter = Arc::new(AtomicU64::new(0));
            let cnt = counter.clone();
            last.watch(move |_| {
                cnt.fetch_add(1, Ordering::Relaxed);
            });

            let mut i = 0u64;
            b.iter(|| {
                i += 1;
                source.set(black_box(i));
            });
        });
    }
    group.finish();
}

fn bench_fan_out(c: &mut Criterion) {
    let mut group = c.benchmark_group("fan out");

    for num_subscribers in [10, 100, 1000, 5000].iter() {
        group.bench_with_input(
            BenchmarkId::new("subscribers", num_subscribers),
            num_subscribers,
            |b, &num_subscribers| {
                let source = Cell::new(0u64);
                let counter = Arc::new(AtomicU64::new(0));

                for _ in 0..num_subscribers {
                    let cnt = counter.clone();
                    source.watch(move |_| {
                        cnt.fetch_add(1, Ordering::Relaxed);
                    });
                }

                let mut i = 0u64;
                b.iter(|| {
                    i += 1;
                    source.set(black_box(i));
                });
            },
        );
    }
    group.finish();
}

fn bench_fan_in(c: &mut Criterion) {
    let mut group = c.benchmark_group("fan in (many sources, one sink)");

    for num_sources in [10, 100, 1000].iter() {
        group.bench_with_input(
            BenchmarkId::new("sources", num_sources),
            num_sources,
            |b, &num_sources| {
                let sources: Vec<_> = (0..num_sources).map(|_| Cell::new(0u64)).collect();
                let counter = Arc::new(AtomicU64::new(0));

                // Each source maps and the watcher counts
                for source in &sources {
                    let cnt = counter.clone();
                    let mapped = source.map(|x| x * 2);
                    mapped.watch(move |_| {
                        cnt.fetch_add(1, Ordering::Relaxed);
                    });
                }

                let mut i = 0u64;
                b.iter(|| {
                    i += 1;
                    // Update all sources
                    for source in &sources {
                        source.set(black_box(i));
                    }
                });
            },
        );
    }
    group.finish();
}

fn bench_complex_graph(c: &mut Criterion) {
    c.bench_function("complex graph: 100 sources -> map -> filter -> scan -> 10 watchers each", |b| {
        let sources: Vec<_> = (0..100).map(|i| Cell::new(i as u64)).collect();
        let counter = Arc::new(AtomicU64::new(0));

        for source in &sources {
            let pipeline = source
                .map(|x| x * 2)
                .filter(|x| x % 2 == 0)
                .scan(0u64, |acc, x| acc + x);

            for _ in 0..10 {
                let cnt = counter.clone();
                pipeline.watch(move |_| {
                    cnt.fetch_add(1, Ordering::Relaxed);
                });
            }
        }

        let mut i = 0u64;
        b.iter(|| {
            i += 1;
            for source in &sources {
                source.set(black_box(i));
            }
        });
    });
}

fn bench_pairwise_chain(c: &mut Criterion) {
    use rx3::PairwiseExt;

    c.bench_function("pairwise chain depth 10", |b| {
        let source = Cell::new(0u64);

        let p1 = source.pairwise();
        let p2 = p1.map(|(a, b)| a + b);
        let p3 = p2.pairwise();
        let p4 = p3.map(|(a, b)| a + b);
        let p5 = p4.pairwise();

        let counter = Arc::new(AtomicU64::new(0));
        let cnt = counter.clone();
        p5.watch(move |_| {
            cnt.fetch_add(1, Ordering::Relaxed);
        });

        let mut i = 0u64;
        b.iter(|| {
            i += 1;
            source.set(black_box(i));
        });
    });
}

criterion_group!(
    benches,
    bench_single_cell_propagation,
    bench_chain_depth,
    bench_fan_out,
    bench_fan_in,
    bench_complex_graph,
    bench_pairwise_chain,
);
criterion_main!(benches);
