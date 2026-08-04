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
    traits::{LeftJoinExt, ProjectMapExt, SelectExt},
};
use seq_macro::seq;

const DEPTH: usize = 64;
const ROWS: usize = 100;
const DIMENSIONS: usize = 4;

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
            row as u64,
            Arc::new(Record {
                bucket: (row % buckets) as u64,
                checksum: row as u64,
                generation: 0,
            }),
        );
    }

    let dimensions = (0..DIMENSIONS)
        .map(|source| {
            let map = CellMap::new();
            for row in 0..ROWS {
                map.insert(
                    row as u64,
                    Arc::new(Dimension {
                        bucket: (row % buckets) as u64,
                        payload: ((source + 1) as u64).wrapping_mul(10_000) + row as u64,
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
            .project(|id, (record, matches)| {
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
            .project(|id, record| {
                Some((
                    *id,
                    Arc::new(Record {
                        bucket: record.bucket,
                        checksum: record.checksum.rotate_left(7) ^ $salt,
                        generation: record.generation,
                    }),
                ))
            })
            .project(|id, record| {
                Some((
                    *id,
                    Arc::new(Record {
                        bucket: record.bucket,
                        checksum: record.checksum.wrapping_mul(33).wrapping_add($salt),
                        generation: record.generation,
                    }),
                ))
            })
            .project(|id, record| {
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
        .project(|id, record| {
            Some((
                *id,
                Arc::new(Record {
                    bucket: record.bucket,
                    checksum: record.checksum.rotate_left(3),
                    generation: record.generation,
                }),
            ))
        });
    join_dimension!(plan, sources.dimensions[0], 1);
    join_dimension!(plan, sources.dimensions[1], 2);
    join_dimension!(plan, sources.dimensions[2], 3);
    join_dimension!(plan, sources.dimensions[3], 4);
    plan.materialize()
}

fn record_value(record: &Option<Arc<Record>>) -> u64 {
    record
        .as_ref()
        .map_or(0, |record| record.checksum ^ record.generation)
}

fn sum_pair((left, right): &(u64, u64)) -> u64 {
    left.wrapping_add(*right).rotate_left(3)
}

fn mix_value(value: &u64) -> u64 {
    value.wrapping_mul(0x9e37_79b9).rotate_left(7)
}

fn fold_value(value: &u64) -> u64 {
    value.rotate_right(11) ^ 0xa076_1d64_78bd_642f
}

fn observe_value(value: &u64) {
    black_box(value);
}

macro_rules! join_view_value {
    ($plan:ident, $view:expr) => {
        let $plan = $plan
            .join(
                $view
                    .get(&0)
                    .map(record_value as fn(&Option<Arc<Record>>) -> u64),
            )
            .map(sum_pair as fn(&(u64, u64)) -> u64)
            .map(mix_value as fn(&u64) -> u64)
            .tap(observe_value as fn(&u64))
            .map(fold_value as fn(&u64) -> u64);
    };
}

fn build_operator_graph(
    views: &[CellMap<u64, Arc<Record>, CellImmutable>],
) -> Cell<u64, CellImmutable> {
    let plan = views[0]
        .get(&0)
        .map(record_value as fn(&Option<Arc<Record>>) -> u64);
    seq!(N in 1..64 {
        join_view_value!(plan, views[N]);
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
