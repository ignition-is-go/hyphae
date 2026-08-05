//! Phase 0 scheduler micro-bench (`--features scheduler`).
//!
//! - `single_cell/synchronous` — one `set` + one subscriber, no batch. The
//!   unscheduled hot path with the feature compiled in; compare against
//!   `latency`'s "single cell set + watch" to confirm the `tick_active` check
//!   doesn't regress the default path.
//! - `diamond/synchronous` vs `diamond/batched` — a diamond whose sink solves
//!   twice per source change synchronously but once under `batch`. Shows the
//!   coalescing win net of batch overhead.

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use hyphae::{Cell, JoinExt, MapExt, Materialize, Mutable, SubscriptionGuard, Watchable, batch};

/// A diamond `s ─> {a, b} ─> j ─> k(solve)` plus the guards keeping it alive.
struct Diamond {
    source: Cell<i64, hyphae::CellMutable>,
    solves: Arc<AtomicU64>,
    _guards: Vec<SubscriptionGuard>,
    _keep: Box<dyn std::any::Any>,
}

/// Stand-in for a real per-emit "solve": a bounded compute the diamond runs
/// once per join fanout. `work == 0` is the trivial (add-only) case; a larger
/// `work` models an rship-style recompute, where the synchronous diamond pays
/// it *twice* (once per leg) and the batched one *once*.
#[inline(never)]
fn solve(x: i64, y: i64, work: u64) -> i64 {
    let mut acc = x.wrapping_add(y);
    for i in 0..work {
        acc = acc
            .wrapping_mul(31)
            .wrapping_add(i64::try_from(i).unwrap_or(i64::MAX) ^ acc);
    }
    black_box(acc)
}

fn build_diamond(work: u64) -> Diamond {
    let source = Cell::new(0i64);
    let add_leg = source.clone().map(|x| x.saturating_add(1)).materialize();
    let multiply_leg = source.clone().map(|x| x.saturating_mul(10)).materialize();
    let joined = add_leg.clone().join(multiply_leg.clone());

    let solves = Arc::new(AtomicU64::new(0));
    let counter = solves.clone();
    let sink = joined
        .map(move |(x, y)| {
            counter.fetch_add(1, Ordering::Relaxed);
            solve(*x, *y, work)
        })
        .materialize();
    let guard = sink.subscribe(|_| {});

    Diamond {
        source,
        solves,
        _guards: vec![guard],
        // Keep the intermediate cells alive so the graph isn't torn down.
        _keep: Box::new((add_leg, multiply_leg, sink)),
    }
}

fn bench_single_cell(c: &mut Criterion) {
    let cell = Cell::new(0i64);
    let hits = Arc::new(AtomicU64::new(0));
    let h = hits;
    let _guard = cell.subscribe(move |_| {
        h.fetch_add(1, Ordering::Relaxed);
    });

    let mut i = 0i64;
    c.bench_function("single_cell/synchronous", |b| {
        b.iter(|| {
            i = i.saturating_add(1);
            cell.set(black_box(i));
        });
    });
}

fn bench_diamond(c: &mut Criterion) {
    let mut group = c.benchmark_group("diamond");

    // `work = 0`: trivial solve — batching only pays overhead (worst case).
    // `work = 4096`: a real per-emit recompute — the synchronous diamond runs
    // it twice, the batched diamond once, so coalescing crosses over to a win.
    for work in [0u64, 4096] {
        let d = build_diamond(work);
        let mut i = 0i64;
        group.bench_function(format!("synchronous/work={work}"), |b| {
            b.iter(|| {
                i = i.saturating_add(1);
                d.source.set(black_box(i));
            });
        });
        black_box(d.solves.load(Ordering::Relaxed));

        let d = build_diamond(work);
        let mut i = 0i64;
        group.bench_function(format!("batched/work={work}"), |b| {
            b.iter(|| {
                i = i.saturating_add(1);
                batch(|| d.source.set(black_box(i)));
            });
        });
        black_box(d.solves.load(Ordering::Relaxed));
    }

    group.finish();
}

criterion_group!(benches, bench_single_cell, bench_diamond);
criterion_main!(benches);
