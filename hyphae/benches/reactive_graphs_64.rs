//! Isolated 64-stage application-shaped reactive graph benchmark.
//!
//! This target is deliberately excluded from default benchmark builds. Its
//! static type contains 64 join stages with four transforms between joins
//! (roughly 316 scalar operators), fed by 64 independently materialized
//! four-join `CellMap` views. Keeping it separate prevents rustc from
//! monomorphizing this type alongside the 160-operator default suite.
//!
//! Run serially to bound compiler memory:
//! `CARGO_BUILD_JOBS=1 cargo bench -p hyphae --bench reactive_graphs_64 --features deep-bench`

#![recursion_limit = "2048"]
#![type_length_limit = "16777216"]

use std::sync::Arc;

use criterion::{BenchmarkId, Criterion, black_box, criterion_group, criterion_main};
use hyphae::{
    Cell, CellImmutable, CellMap, JoinExt, MapExt, MapQuery, Materialize, TapExt,
    traits::{LeftJoinExt, MapEntriesExt, SelectExt},
};
use seq_macro::seq;

const DEPTH: usize = 64;
const ROWS: usize = 100;
const DIMENSIONS: usize = 4;

fn usize_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

fn safe_mod(value: usize, modulus: usize) -> usize {
    value.checked_rem(modulus).unwrap_or(0)
}

#[derive(Clone, Debug, PartialEq)]
struct Record {
    bucket: u64,
    checksum: u64,
    generation: u64,
}

#[derive(Clone, Debug, PartialEq)]
struct Dimension {
    bucket: u64,
    payload: u64,
}

struct Sources {
    root: CellMap<u64, Arc<Record>>,
    dimensions: Vec<CellMap<u64, Arc<Dimension>>>,
}

fn populate() -> Sources {
    let buckets = (ROWS / 8).max(1);
    let root = CellMap::new();
    for row in 0..ROWS {
        root.insert(
            usize_u64(row),
            Arc::new(Record {
                bucket: usize_u64(safe_mod(row, buckets)),
                checksum: usize_u64(row),
                generation: 0,
            }),
        );
    }

    let dimensions = (0..DIMENSIONS)
        .map(|source| {
            let map = CellMap::new();
            for row in 0..ROWS {
                map.insert(
                    usize_u64(row),
                    Arc::new(Dimension {
                        bucket: usize_u64(safe_mod(row, buckets)),
                        payload: usize_u64(source)
                            .saturating_add(1)
                            .wrapping_mul(10_000)
                            .wrapping_add(usize_u64(row)),
                    }),
                );
            }
            map
        })
        .collect();

    Sources { root, dimensions }
}

macro_rules! join_dimension {
    ($plan:ident, $dimension:expr, $salt:expr) => {
        let $plan = $plan
            .left_join_by(
                $dimension.clone(),
                |_id, record| record.bucket,
                |_id, dimension| dimension.bucket,
            )
            .filter_map_entries(|id, (record, matches)| {
                let checksum = matches.iter().fold(record.checksum, |acc, dimension| {
                    acc.rotate_left(5) ^ dimension.payload.wrapping_add($salt)
                });
                Some((
                    *id,
                    Arc::new(Record {
                        bucket: record.bucket,
                        checksum,
                        generation: record.generation,
                    }),
                ))
            })
            .filter_map_entries(|id, record| {
                Some((
                    *id,
                    Arc::new(Record {
                        bucket: record.bucket,
                        checksum: record.checksum.rotate_left(7) ^ $salt,
                        generation: record.generation,
                    }),
                ))
            })
            .filter_map_entries(|id, record| {
                Some((
                    *id,
                    Arc::new(Record {
                        bucket: record.bucket,
                        checksum: record.checksum.wrapping_mul(33).wrapping_add($salt),
                        generation: record.generation,
                    }),
                ))
            })
            .filter_map_entries(|id, record| {
                Some((
                    *id,
                    Arc::new(Record {
                        bucket: record.bucket,
                        checksum: record.checksum.rotate_right(11) ^ ($salt << 1),
                        generation: record.generation,
                    }),
                ))
            });
    };
}

fn build_view(sources: &Sources) -> CellMap<u64, Arc<Record>, CellImmutable> {
    let plan = sources
        .root
        .clone()
        .select(|record| record.bucket % 2 == 0 || record.generation % 2 == 1)
        .filter_map_entries(|id, record| {
            Some((
                *id,
                Arc::new(Record {
                    bucket: record.bucket,
                    checksum: record.checksum.rotate_left(3),
                    generation: record.generation,
                }),
            ))
        });
    let dimension = sources
        .dimensions
        .first()
        .cloned()
        .unwrap_or_else(CellMap::new);
    join_dimension!(plan, dimension, 1);
    let dimension = sources
        .dimensions
        .get(1)
        .cloned()
        .unwrap_or_else(CellMap::new);
    join_dimension!(plan, dimension, 2);
    let dimension = sources
        .dimensions
        .get(2)
        .cloned()
        .unwrap_or_else(CellMap::new);
    join_dimension!(plan, dimension, 3);
    let dimension = sources
        .dimensions
        .get(3)
        .cloned()
        .unwrap_or_else(CellMap::new);
    join_dimension!(plan, dimension, 4);
    plan.materialize()
}

macro_rules! join_view_value {
    ($plan:ident, $view:expr) => {
        let $plan = $plan
            .materialize()
            .join(
                $view
                    .get(&0)
                    .map(|record| {
                        record
                            .as_ref()
                            .map_or(0, |record| record.checksum ^ record.generation)
                    })
                    .materialize(),
            )
            .map(|(left, right)| left.wrapping_add(*right).rotate_left(3))
            .map(|value| value.wrapping_mul(0x9e37_79b9).rotate_left(7))
            .tap(|value| {
                black_box(value);
            })
            .map(|value| value.rotate_right(11) ^ 0xa076_1d64_78bd_642f);
    };
}

fn build_operator_graph(
    views: &[CellMap<u64, Arc<Record>, CellImmutable>],
) -> Cell<u64, CellImmutable> {
    let first_view = views
        .first()
        .cloned()
        .unwrap_or_else(|| CellMap::new().lock());
    let plan = first_view.get(&0).map(|record| {
        record
            .as_ref()
            .map_or(0, |record| record.checksum ^ record.generation)
    });
    seq!(N in 1..64 {
        let view = views.get(N).cloned().unwrap_or_else(|| CellMap::new().lock());
        join_view_value!(plan, view);
    });
    plan.materialize()
}

fn mutate_root(sources: &Sources, tick: u64) {
    sources.root.insert(
        0,
        Arc::new(Record {
            bucket: 0,
            checksum: black_box(tick.wrapping_mul(17)),
            generation: tick,
        }),
    );
}

fn bench_operator_graph(c: &mut Criterion) {
    let mut group = c.benchmark_group("reactive_graph/join_heavy_operator_pipeline");
    group.sample_size(30);
    group.bench_with_input(BenchmarkId::from_parameter(DEPTH), &DEPTH, |b, _| {
        let sources = populate();
        let views: Vec<_> = (0..DEPTH).map(|_| build_view(&sources)).collect();
        let output = build_operator_graph(&views);
        let mut tick = 0u64;
        b.iter(|| {
            tick = tick.wrapping_add(1);
            mutate_root(&sources, tick);
            black_box(&output);
        });
    });
    group.finish();
}

criterion_group!(benches, bench_operator_graph);
criterion_main!(benches);
