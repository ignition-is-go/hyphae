//! Construction cost of small reactive graphs.
//!
//! Servers build thousands of small graphs (a few dozen cells each) on one
//! thread, so installing and materializing a plan is a hot path of its own.
//! Each case builds a graph over an existing source and keeps it alive until
//! after timing, so teardown is excluded.

use std::sync::Arc;

use criterion::{BenchmarkId, Criterion, black_box, criterion_group, criterion_main};
use hyphae::{
    Cell, CellMap, CountByExt, InnerJoinExt, MapEntriesExt, MapExt, MapQuery, MapValuesExt,
    Materialize, SwitchMapExt,
};

const SIZES: [u64; 2] = [10, 200];

#[derive(Clone, Debug, PartialEq)]
struct Row {
    relation: u64,
    payload: u64,
}

fn rows(n: u64) -> CellMap<u64, Arc<Row>> {
    let map = CellMap::new();
    for key in 0..n {
        map.insert(
            key,
            Arc::new(Row {
                relation: key % 8,
                payload: key.wrapping_mul(17),
            }),
        );
    }
    map
}

fn bench_cells(c: &mut Criterion) {
    let source = Cell::new(1_u64);
    c.bench_function("graph_construction/map_chain", |b| {
        b.iter_with_large_drop(|| {
            source
                .clone()
                .map(|v| v.wrapping_add(1))
                .map(|v| v.wrapping_mul(3))
                .map(|v| v ^ 7)
                .materialize()
        });
    });

    let outer = Cell::new(1_u64);
    let inner = Cell::new(2_u64);
    c.bench_function("graph_construction/switch_map", |b| {
        b.iter_with_large_drop(|| {
            let inner = inner.clone();
            outer
                .clone()
                .switch_map(move |_| inner.clone())
                .materialize()
        });
    });
}

fn bench_queries(c: &mut Criterion) {
    let mut group = c.benchmark_group("graph_construction");
    for n in SIZES {
        let source = rows(n);
        let right = rows(n);
        group.bench_with_input(BenchmarkId::new("map_values", n), &n, |b, _| {
            b.iter_with_large_drop(|| {
                source
                    .clone()
                    .map_values(|_, row| row.payload.wrapping_add(1))
                    .materialize()
            });
        });
        group.bench_with_input(BenchmarkId::new("single", n), &n, |b, _| {
            b.iter_with_large_drop(|| {
                source
                    .clone()
                    .map_entries(|key, row| (key.wrapping_add(1), row.payload))
                    .materialize()
            });
        });
        group.bench_with_input(BenchmarkId::new("diff", n), &n, |b, _| {
            b.iter_with_large_drop(|| source.clone().count_by(|_, row| row.relation).materialize());
        });
        group.bench_with_input(BenchmarkId::new("pair_join", n), &n, |b, _| {
            b.iter_with_large_drop(|| {
                source
                    .clone()
                    .inner_join_by(
                        right.clone(),
                        |key, _| *key,
                        |_, row: &Arc<Row>| row.payload.checked_div(17).unwrap_or(0),
                    )
                    .materialize()
            });
        });
        group.bench_with_input(BenchmarkId::new("composite", n), &n, |b, _| {
            b.iter_with_large_drop(|| {
                let a = source
                    .clone()
                    .map_values(|_, row| row.payload)
                    .materialize();
                let b2 = source
                    .clone()
                    .map_entries(|key, row| (*key, row.relation))
                    .materialize();
                let c2 = source.clone().count_by(|_, row| row.relation).materialize();
                black_box((a, b2, c2))
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_cells, bench_queries);
criterion_main!(benches);
