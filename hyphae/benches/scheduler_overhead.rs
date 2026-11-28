//! Fixed per-run and per-wave scheduler cost, measured where it shows up in a
//! real server: a child scheduling domain whose timer-like source is written
//! once per tick and fans out to many small chains.
//!
//! - `timer_fanout/{N}` — one child-domain source feeding `N` chains of two
//!   materialized maps and a counting subscriber. One iteration is one tick:
//!   a batched write to the source, drained on the calling thread. Three
//!   waves of width 1, `N` and `N`, so the user closures are trivial and the
//!   time is almost all enqueue, pop and dispatch.
//! - `mixed_heights/{N}` — `N` child-domain sources written in one batch, the
//!   `i`th feeding a chain of `i % 8 + 1` maps. Many trivial runs spread over
//!   eight heights of shrinking width.

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use criterion::{BenchmarkId, Criterion, Throughput, black_box, criterion_group, criterion_main};
use hyphae::{
    Cell, CellImmutable, CellMutable, MapExt, Materialize, Mutable, SubscriptionGuard, Watchable,
    scheduler::Domain,
};

const WIDTHS: [usize; 3] = [100, 500, 2000];

struct Graph {
    sources: Vec<Cell<u64, CellMutable>>,
    hits: Arc<AtomicU64>,
    _cells: Vec<Cell<u64, CellImmutable>>,
    _guards: Vec<SubscriptionGuard>,
}

/// Chains rooted at `sources[i % sources.len()]`, each `depth(i)` maps long,
/// ending in a subscriber that counts deliveries.
fn build(domain: &Domain, sources: usize, chains: usize, depth: impl Fn(usize) -> usize) -> Graph {
    domain.enter(|| {
        let sources: Vec<_> = (0..sources).map(|_| Cell::new(0u64)).collect();
        let hits = Arc::new(AtomicU64::new(0));
        let mut cells = Vec::new();
        let mut guards = Vec::new();
        for chain in 0..chains {
            let Some(source) = sources.get(chain.checked_rem(sources.len()).unwrap_or(0)) else {
                continue;
            };
            let offset = u64::try_from(chain).unwrap_or(u64::MAX);
            let mut tail = source
                .clone()
                .map(move |x| x.wrapping_add(offset))
                .materialize();
            for _ in 1..depth(chain) {
                tail = tail.map(|x| x.wrapping_mul(3)).materialize();
                cells.push(tail.clone());
            }
            let counter = Arc::clone(&hits);
            guards.push(tail.subscribe(move |_| {
                counter.fetch_add(1, Ordering::Relaxed);
            }));
            cells.push(tail);
        }
        Graph {
            sources,
            hits,
            _cells: cells,
            _guards: guards,
        }
    })
}

fn tick(domain: &Domain, graph: &Graph, value: u64) {
    domain.batch(|| {
        for source in &graph.sources {
            source.set(value);
        }
    });
}

fn bench_timer_fanout(c: &mut Criterion) {
    let domain = Domain::child();
    let mut group = c.benchmark_group("timer_fanout");
    for width in WIDTHS {
        let graph = build(&domain, 1, width, |_| 2);
        group.throughput(Throughput::Elements(
            u64::try_from(width).unwrap_or(u64::MAX),
        ));
        let mut value = 0u64;
        group.bench_with_input(BenchmarkId::from_parameter(width), &width, |b, _| {
            b.iter(|| {
                value = value.wrapping_add(1);
                tick(&domain, &graph, black_box(value));
            });
        });
        black_box(graph.hits.load(Ordering::Relaxed));
    }
    group.finish();
}

fn bench_mixed_heights(c: &mut Criterion) {
    let domain = Domain::child();
    let mut group = c.benchmark_group("mixed_heights");
    for width in WIDTHS {
        let graph = build(&domain, width, width, |chain| {
            chain.checked_rem(8).unwrap_or(0).saturating_add(1)
        });
        group.throughput(Throughput::Elements(
            u64::try_from(width).unwrap_or(u64::MAX),
        ));
        let mut value = 0u64;
        group.bench_with_input(BenchmarkId::from_parameter(width), &width, |b, _| {
            b.iter(|| {
                value = value.wrapping_add(1);
                tick(&domain, &graph, black_box(value));
            });
        });
        black_box(graph.hits.load(Ordering::Relaxed));
    }
    group.finish();
}

criterion_group!(benches, bench_timer_fanout, bench_mixed_heights);
criterion_main!(benches);
