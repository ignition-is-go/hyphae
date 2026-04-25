// Pipeline types nest one level per operator; `bench_chain_depth` builds
// up to 100 layers. Default trait-solver recursion limit (128) is borderline.
#![recursion_limit = "2048"]
#![type_length_limit = "16777216"]

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use criterion::{BenchmarkId, Criterion, black_box, criterion_group, criterion_main};
use hyphae::{Cell, FilterExt, MapExt, Mutable, ParallelExt, Pipeline, ScanExt, Signal, Watchable};
use seq_macro::seq;

fn bench_single_cell_propagation(c: &mut Criterion) {
    c.bench_function("single cell set + watch", |b| {
        let cell = Cell::new(0u64);
        let counter = Arc::new(AtomicU64::new(0));
        let cnt = counter.clone();
        let _guard = cell.subscribe(move |_| {
            cnt.fetch_add(1, Ordering::Relaxed);
        });

        b.iter(|| {
            cell.set(black_box(42));
        });
    });
}

/// Build a `bench_chain_depth` "map chain" sub-bench labeled by depth.
///
/// Post-Pipeline-refactor each `.map` returns a distinct `MapPipeline` type,
/// so the chain is constructed at compile time (one map per token) and
/// materialized into a single `Cell` with one root subscription.
///
/// $tail = $depth - 1.
macro_rules! map_chain_bench {
    ($group:expr, $depth:literal, $tail:literal) => {
        $group.bench_with_input(
            BenchmarkId::new("map chain", $depth as u32),
            &($depth as u32),
            |b, _| {
                let source = Cell::new(0u64);
                let chain = seq!(_N in 0..$tail {
                    source.map(|x| x + 1) #(.map(|x| x + 1))*
                })
                .materialize();

                let counter = Arc::new(AtomicU64::new(0));
                let cnt = counter.clone();
                let _guard = chain.subscribe(move |_| {
                    cnt.fetch_add(1, Ordering::Relaxed);
                });

                let mut i = 0u64;
                b.iter(|| {
                    i += 1;
                    source.set(black_box(i));
                });
            },
        );
    };
}

fn bench_chain_depth(c: &mut Criterion) {
    let mut group = c.benchmark_group("chain depth propagation");

    map_chain_bench!(group, 1, 0);
    map_chain_bench!(group, 5, 4);
    map_chain_bench!(group, 10, 9);
    map_chain_bench!(group, 20, 19);
    map_chain_bench!(group, 50, 49);
    map_chain_bench!(group, 100, 99);

    group.finish();
}

fn bench_fan_out(c: &mut Criterion) {
    let mut group = c.benchmark_group("fan out");

    for num_subscribers in [10, 100, 1000, 5000].iter() {
        group.bench_with_input(
            BenchmarkId::new("sequential", num_subscribers),
            num_subscribers,
            |b, &num_subscribers| {
                let source = Cell::new(0u64);
                let counter = Arc::new(AtomicU64::new(0));

                let guards: Vec<_> = (0..num_subscribers)
                    .map(|_| {
                        let cnt = counter.clone();
                        source.subscribe(move |_| {
                            cnt.fetch_add(1, Ordering::Relaxed);
                        })
                    })
                    .collect();

                let mut i = 0u64;
                b.iter(|| {
                    i += 1;
                    source.set(black_box(i));
                });

                drop(guards);
            },
        );

        group.bench_with_input(
            BenchmarkId::new("parallel", num_subscribers),
            num_subscribers,
            |b, &num_subscribers| {
                let source = Cell::new(0u64);
                let parallel = source.parallel();
                let counter = Arc::new(AtomicU64::new(0));

                let guards: Vec<_> = (0..num_subscribers)
                    .map(|_| {
                        let cnt = counter.clone();
                        parallel.subscribe(move |_| {
                            cnt.fetch_add(1, Ordering::Relaxed);
                        })
                    })
                    .collect();

                let mut i = 0u64;
                b.iter(|| {
                    i += 1;
                    source.set(black_box(i));
                });

                drop(guards);
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
                let guards: Vec<_> = sources
                    .iter()
                    .map(|source| {
                        let cnt = counter.clone();
                        let mapped = source.map(|x| x * 2).materialize();
                        mapped.subscribe(move |_| {
                            cnt.fetch_add(1, Ordering::Relaxed);
                        })
                    })
                    .collect();

                let mut i = 0u64;
                b.iter(|| {
                    i += 1;
                    // Update all sources
                    for source in &sources {
                        source.set(black_box(i));
                    }
                });

                drop(guards);
            },
        );
    }
    group.finish();
}

fn bench_complex_graph(c: &mut Criterion) {
    c.bench_function(
        "complex graph: 100 sources -> map -> filter -> scan -> 10 watchers each",
        |b| {
            let sources: Vec<_> = (0..100).map(|i| Cell::new(i as u64)).collect();
            let counter = Arc::new(AtomicU64::new(0));

            let guards: Vec<_> = sources
                .iter()
                .flat_map(|source| {
                    // scan requires Watchable so the pure pipeline must be
                    // materialized before scan. The post-Pipeline-refactor
                    // shape: one fused (map.filter) cell, then scan on top.
                    let filtered = source
                        .map(|x| x * 2)
                        .filter(|x| x % 2 == 0)
                        .materialize();
                    let pipeline = filtered.scan(0u64, |acc, x| acc + x);

                    let counter = counter.clone();
                    (0..10)
                        .map(move |_| {
                            let cnt = counter.clone();
                            pipeline.subscribe(move |_| {
                                cnt.fetch_add(1, Ordering::Relaxed);
                            })
                        })
                        .collect::<Vec<_>>()
                })
                .collect();

            let mut i = 0u64;
            b.iter(|| {
                i += 1;
                for source in &sources {
                    source.set(black_box(i));
                }
            });

            drop(guards);
        },
    );
}

fn bench_pairwise_chain(c: &mut Criterion) {
    use hyphae::PairwiseExt;

    c.bench_function("pairwise chain depth 10", |b| {
        let source = Cell::new(0u64);

        // pairwise returns a Cell; map returns a Pipeline. To pairwise the
        // mapped output we must materialize between each map and the next
        // pairwise (pairwise requires Watchable, which only Cell implements).
        let p1 = source.pairwise();
        let p2 = p1.map(|(a, b)| a + b).materialize();
        let p3 = p2.pairwise();
        let p4 = p3.map(|(a, b)| a + b).materialize();
        let p5 = p4.pairwise();

        let counter = Arc::new(AtomicU64::new(0));
        let cnt = counter.clone();
        let _guard = p5.subscribe(move |_| {
            cnt.fetch_add(1, Ordering::Relaxed);
        });

        let mut i = 0u64;
        b.iter(|| {
            i += 1;
            source.set(black_box(i));
        });
    });
}

fn bench_parallel_heavy_callbacks(c: &mut Criterion) {
    let mut group = c.benchmark_group("parallel heavy callbacks");

    for num_subscribers in [10, 100, 500].iter() {
        group.bench_with_input(
            BenchmarkId::new("sequential", num_subscribers),
            num_subscribers,
            |b, &num_subscribers| {
                let source = Cell::new(0u64);
                let results: Vec<_> = (0..num_subscribers)
                    .map(|_| Arc::new(AtomicU64::new(0)))
                    .collect();

                let guards: Vec<_> = results
                    .iter()
                    .map(|result| {
                        let r = result.clone();
                        source.subscribe(move |signal| {
                            if let Signal::Value(x) = signal {
                                // Simulate expensive work - can't be optimized away
                                let mut sum = **x;
                                for _ in 0..10000 {
                                    sum = sum.wrapping_mul(31).wrapping_add(17);
                                }
                                r.store(sum, Ordering::Relaxed);
                            }
                        })
                    })
                    .collect();

                let mut i = 0u64;
                b.iter(|| {
                    i += 1;
                    source.set(black_box(i));
                });

                drop(guards);
            },
        );

        group.bench_with_input(
            BenchmarkId::new("parallel", num_subscribers),
            num_subscribers,
            |b, &num_subscribers| {
                let source = Cell::new(0u64);
                let parallel = source.parallel();
                let results: Vec<_> = (0..num_subscribers)
                    .map(|_| Arc::new(AtomicU64::new(0)))
                    .collect();

                let guards: Vec<_> = results
                    .iter()
                    .map(|result| {
                        let r = result.clone();
                        parallel.subscribe(move |signal| {
                            if let Signal::Value(x) = signal {
                                // Simulate expensive work - can't be optimized away
                                let mut sum = **x;
                                for _ in 0..10000 {
                                    sum = sum.wrapping_mul(31).wrapping_add(17);
                                }
                                r.store(sum, Ordering::Relaxed);
                            }
                        })
                    })
                    .collect();

                let mut i = 0u64;
                b.iter(|| {
                    i += 1;
                    source.set(black_box(i));
                });

                drop(guards);
            },
        );
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_single_cell_propagation,
    bench_chain_depth,
    bench_fan_out,
    bench_fan_in,
    bench_complex_graph,
    bench_pairwise_chain,
    bench_parallel_heavy_callbacks,
);
criterion_main!(benches);
