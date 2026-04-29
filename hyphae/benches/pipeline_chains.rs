// Pipeline types nest one level per operator. At depth 500 the trait solver
// must walk ~500 layers of `MapPipeline<...>` to verify `Pipeline<T>` bounds;
// the default recursion limit (128) is insufficient.
#![recursion_limit = "2048"]
#![type_length_limit = "16777216"]

//! Benchmarks targeting the Pipeline migration.
//!
//! Run with `cargo bench --bench pipeline_chains -- --save-baseline pre-pipeline`
//! BEFORE the refactor, then re-run with `--baseline pre-pipeline` after each
//! task in `docs/superpowers/plans/2026-04-23-pipeline-type.md` to track effect.
//!
//! What we expect to see:
//! - **pure_chain_set**: pre-refactor scales linearly with chain depth because
//!   each `.map(...)` allocates a `Cell` with its own subscriber table; after
//!   the refactor a chain of pure operators fuses into one closure on the root
//!   cell, so per-set cost should be roughly constant in chain depth.
//! - **mixed_chain_set**: same story for mixed pure operators (map/filter/tap).
//! - **chain_construction**: pre-refactor allocates one cell + subscriber map
//!   per `.map`; after, allocating only closures should drop construction cost
//!   substantially at large depths.
//! - **filter_blocking_chain_set**: shows whether intermediate filter cells
//!   add overhead even when the filter blocks. Post-refactor the predicate
//!   runs inside the fused closure with no cell-level work.
//!
//! # Depth construction
//!
//! Post-refactor, each `.map(...)` returns a distinct `MapPipeline<S, T, U, F>`
//! type. Loop reassignment (`last = last.map(...)`) no longer typechecks. We
//! use `seq_macro::seq!` to build chains of a fixed depth at compile time so
//! every operator's distinct closure type is preserved (chain fusion is a
//! property of the static type). Each depth gets its own bench function.

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use criterion::{BenchmarkId, Criterion, black_box, criterion_group, criterion_main};
use hyphae::{
    Cell, FilterExt, MapExt, MaterializeDefinite, MaterializeEmpty, Mutable, TapExt, Watchable,
};
use seq_macro::seq;

/// Reference point: one source cell, one subscriber, no chain.
///
/// Pre-refactor this is the per-stage ArcSwap cost the chain benches multiply
/// by depth. Post-refactor a fused chain should converge toward this number
/// regardless of chain depth, because all intermediate ArcSwap stores have
/// been eliminated.
fn bench_baseline_one_subscriber(c: &mut Criterion) {
    c.bench_function("baseline_one_subscriber", |b| {
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

/// Build a `pure_chain_set` bench at fixed depth N.
///
/// `seq!(_N in 0..(N-1) { source.map(...) #(.map(...))* })` emits one base map
/// plus N-1 additional `.map`s for a total of N stages, all fused into one
/// pipeline. `.materialize()` installs a single subscription on the source.
///
/// The bench label is `pure_chain_set/N` (the chain depth) so criterion can
/// match against the pre-refactor baseline.
macro_rules! bench_pure_chain {
    ($group:expr, $depth:literal, $tail:literal) => {
        $group.bench_with_input(
            BenchmarkId::from_parameter($depth as u32),
            &($depth as u32),
            |b, _| {
                let source = Cell::new(0u64);
                let chain = seq!(_N in 0..$tail {
                    source.clone().map(|x| x + 1) #(.map(|x| x + 1))*
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

fn bench_pure_map_chain_set(c: &mut Criterion) {
    let mut group = c.benchmark_group("pure_chain_set");
    // (depth, seq_tail_count) — tail = depth - 1.
    bench_pure_chain!(group, 1, 0);
    bench_pure_chain!(group, 5, 4);
    bench_pure_chain!(group, 25, 24);
    bench_pure_chain!(group, 100, 99);
    bench_pure_chain!(group, 250, 249);
    bench_pure_chain!(group, 500, 499);
    group.finish();
}

/// Build a `mixed_chain_set` bench at $cycles cycles. One cycle is
/// `.map.filter.map.tap` (4 ops). Total chain length is `1 + 4 * cycles` ops.
macro_rules! bench_mixed_chain {
    ($group:expr, $cycles:literal) => {
        $group.bench_with_input(
            BenchmarkId::from_parameter($cycles as u32),
            &($cycles as u32),
            |b, _| {
                let source = Cell::new(0u64);
                let chain = seq!(_N in 0..$cycles {
                    source.clone().map(|x| x + 1)
                    #(
                        .map(|x| x + 1)
                        .filter(|_| true)
                        .map(|x| x + 1)
                        .tap(|_| {})
                    )*
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

fn bench_mixed_pure_chain_set(c: &mut Criterion) {
    let mut group = c.benchmark_group("mixed_chain_set");
    bench_mixed_chain!(group, 1);
    bench_mixed_chain!(group, 5);
    bench_mixed_chain!(group, 25);
    bench_mixed_chain!(group, 100);
    group.finish();
}

/// Build a `chain_construction` bench labeled by chain depth.
///
/// Measures cost of building (and dropping) the pipeline; no subscription.
/// $tail = $depth - 1.
macro_rules! bench_construction {
    ($group:expr, $depth:literal, $tail:literal) => {
        $group.bench_with_input(
            BenchmarkId::from_parameter($depth as u32),
            &($depth as u32),
            |b, _| {
                b.iter(|| {
                    let source = Cell::new(0u64);
                    let chain = seq!(_N in 0..$tail {
                        source.clone().map(|x| x + 1) #(.map(|x| x + 1))*
                    });
                    black_box(chain);
                    black_box(source);
                });
            },
        );
    };
}

fn bench_chain_construction(c: &mut Criterion) {
    let mut group = c.benchmark_group("chain_construction");
    bench_construction!(group, 10, 9);
    bench_construction!(group, 50, 49);
    bench_construction!(group, 250, 249);
    bench_construction!(group, 500, 499);
    group.finish();
}

/// Build a `filter_blocking_chain_set` bench labeled `depth=$depth`.
///
/// Mirrors the pre-refactor `for _ in 1..depth { last = last.map.filter }`
/// pattern: one base map followed by `depth - 1` `.map.filter` cycles. Filter
/// predicate is `x % 2 == 0` so half the emissions are blocked.
///
/// $tail = $depth - 1.
macro_rules! bench_filter_blocking {
    ($group:expr, $depth:literal, $tail:literal) => {
        $group.bench_with_input(
            BenchmarkId::from_parameter($depth as u32),
            &($depth as u32),
            |b, _| {
                let source = Cell::new(0u64);
                let chain = seq!(_N in 0..$tail {
                    source.clone().map(|x| x + 1)
                    #(
                        .map(|x| x + 1)
                        .filter(|x| x % 2 == 0)
                    )*
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

fn bench_filter_blocking_chain_set(c: &mut Criterion) {
    let mut group = c.benchmark_group("filter_blocking_chain_set");
    bench_filter_blocking!(group, 5, 4);
    bench_filter_blocking!(group, 25, 24);
    bench_filter_blocking!(group, 100, 99);
    group.finish();
}

criterion_group!(
    benches,
    bench_baseline_one_subscriber,
    bench_pure_map_chain_set,
    bench_mixed_pure_chain_set,
    bench_chain_construction,
    bench_filter_blocking_chain_set,
);
criterion_main!(benches);
