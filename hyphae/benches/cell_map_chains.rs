//! Benchmarks targeting the CellMap query-plans migration.
//!
//! Run BEFORE the refactor:
//!   cargo bench --bench cell_map_chains -- --save-baseline pre-cellmap-query-plans
//!
//! Re-run after each port task with --baseline pre-cellmap-query-plans to
//! track effect on chain propagation cost.
//!
//! Each `inner_join` produces tuple values of type `(LV, RV)` so the chain's
//! value type compounds with depth. We hardcode a few representative depths
//! rather than loop, so closures' nested types resolve cleanly.
//!
//! Post-refactor, `inner_join` consumes `self`, takes the right side by
//! value, and returns a plan node. The whole chain is materialized once at
//! the end via `MapQuery::materialize`, replacing the per-stage CellMap
//! allocation + ArcSwap subscription that the old API forced.

use criterion::{BenchmarkId, Criterion, black_box, criterion_group, criterion_main};
use hyphae::{CellMap, MapQuery, traits::InnerJoinExt};

fn make_source(seed: i64) -> CellMap<String, i64> {
    let m = CellMap::<String, i64>::new();
    for i in 0..16i64 {
        m.insert(format!("k{}", i), seed + i);
    }
    m
}

fn bench_depth_1(c: &mut Criterion) {
    let mut group = c.benchmark_group("join_chain_set");
    group.bench_with_input(BenchmarkId::from_parameter(1u32), &1u32, |b, _| {
        let a = make_source(0);
        let b_src = make_source(100);
        let _chain = a.clone().inner_join(b_src.clone()).materialize();
        let mut i = 0i64;
        b.iter(|| {
            i = i.wrapping_add(1);
            a.insert(format!("k{}", black_box(0)), black_box(i));
        });
    });
    group.finish();
}

fn bench_depth_2(c: &mut Criterion) {
    let mut group = c.benchmark_group("join_chain_set");
    group.bench_with_input(BenchmarkId::from_parameter(2u32), &2u32, |b, _| {
        let a = make_source(0);
        let b_src = make_source(100);
        let c_src = make_source(200);
        let _chain = a
            .clone()
            .inner_join(b_src.clone())
            .inner_join(c_src.clone())
            .materialize();
        let mut i = 0i64;
        b.iter(|| {
            i = i.wrapping_add(1);
            a.insert(format!("k{}", black_box(0)), black_box(i));
        });
    });
    group.finish();
}

fn bench_depth_3(c: &mut Criterion) {
    let mut group = c.benchmark_group("join_chain_set");
    group.bench_with_input(BenchmarkId::from_parameter(3u32), &3u32, |b, _| {
        let a = make_source(0);
        let b_src = make_source(100);
        let c_src = make_source(200);
        let d_src = make_source(300);
        let _chain = a
            .clone()
            .inner_join(b_src.clone())
            .inner_join(c_src.clone())
            .inner_join(d_src.clone())
            .materialize();
        let mut i = 0i64;
        b.iter(|| {
            i = i.wrapping_add(1);
            a.insert(format!("k{}", black_box(0)), black_box(i));
        });
    });
    group.finish();
}

fn bench_depth_5(c: &mut Criterion) {
    let mut group = c.benchmark_group("join_chain_set");
    group.bench_with_input(BenchmarkId::from_parameter(5u32), &5u32, |b, _| {
        let a = make_source(0);
        let b_src = make_source(100);
        let c_src = make_source(200);
        let d_src = make_source(300);
        let e_src = make_source(400);
        let f_src = make_source(500);
        let _chain = a
            .clone()
            .inner_join(b_src.clone())
            .inner_join(c_src.clone())
            .inner_join(d_src.clone())
            .inner_join(e_src.clone())
            .inner_join(f_src.clone())
            .materialize();
        let mut i = 0i64;
        b.iter(|| {
            i = i.wrapping_add(1);
            a.insert(format!("k{}", black_box(0)), black_box(i));
        });
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_depth_1,
    bench_depth_2,
    bench_depth_3,
    bench_depth_5
);
criterion_main!(benches);
