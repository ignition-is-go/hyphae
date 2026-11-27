//! Construction cost of small reactive graphs.
//!
//! Servers build thousands of small graphs (a few dozen cells each) on one
//! thread, so installing and materializing a plan is a hot path of its own.
//! Each case builds a graph over an existing source and keeps it alive until
//! after timing, so teardown is excluded.

use std::sync::Arc;

use criterion::{BatchSize, BenchmarkId, Criterion, black_box, criterion_group, criterion_main};
use hyphae::{
    Cell, CellMap, CountByExt, InnerJoinExt, LeftJoinExt, MapEntriesExt, MapExt, MapQuery,
    MapValuesExt, Materialize, SwitchMapExt,
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

/// One join key holding every left row: the shape of all nodes of one engine
/// joined to that engine. Index maintenance must stay linear in the bucket.
fn bench_hot_join_key(c: &mut Criterion) {
    let mut group = c.benchmark_group("graph_construction/hot_join_key");
    group.sample_size(10);
    for n in [1_000_u64, 10_000] {
        group.bench_with_input(BenchmarkId::new("insert_remove", n), &n, |b, &n| {
            b.iter_batched(
                || {
                    let left = CellMap::<u64, Arc<Row>>::new();
                    let output = left
                        .clone()
                        .left_join_by(rows(1), |_, _| 0_u64, |key, _| *key)
                        .map_joined_values(|_, row, matches| (row.payload, matches.len()))
                        .materialize();
                    (left, output)
                },
                |(left, output)| {
                    for key in 0..n {
                        left.insert(
                            key,
                            Arc::new(Row {
                                relation: 0,
                                payload: key,
                            }),
                        );
                    }
                    for key in 0..n {
                        left.remove(&key);
                    }
                    (left, output)
                },
                BatchSize::PerIteration,
            );
        });
    }
    group.finish();
}

/// One right-side join key holding every right row: each change hands the
/// closure the whole matched group, so group maintenance must not rescan it.
fn bench_hot_right_group(c: &mut Criterion) {
    let mut group = c.benchmark_group("graph_construction/hot_right_group");
    group.sample_size(10);
    for n in [1_000_u64, 10_000] {
        group.bench_with_input(BenchmarkId::new("insert_update_remove", n), &n, |b, &n| {
            b.iter_batched(
                || {
                    let right = CellMap::<u64, Arc<Row>>::new();
                    let output = rows(1)
                        .left_join_by(right.clone(), |_, _| 0_u64, |_, row| row.relation)
                        .map_joined_values(|_, row, matches| (row.payload, matches.len()))
                        .materialize();
                    (right, output)
                },
                |(right, output)| {
                    for round in 0..2 {
                        for key in 0..n {
                            right.insert(
                                key,
                                Arc::new(Row {
                                    relation: 0,
                                    payload: key.wrapping_add(round),
                                }),
                            );
                        }
                    }
                    for key in 0..n {
                        right.remove(&key);
                    }
                    (right, output)
                },
                BatchSize::PerIteration,
            );
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_cells,
    bench_queries,
    bench_hot_join_key,
    bench_hot_right_group
);
criterion_main!(benches);
