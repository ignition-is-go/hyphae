//! Deep and wide application-shaped reactive graph benchmarks.
//!
//! This suite is intentionally dominated by `CellMap` query plans: every
//! materialized view joins 4, 8, or 12 independently mutable data sources and
//! performs a projection after every join. It measures updates entering at the
//! root, updates entering at the deepest source, waves touching every source,
//! graph installation/teardown, row scaling, and observer fan-out.
//!
//! Keep benchmark names and workloads stable across the pre/post migration
//! revisions. The implementation syntax may differ to accommodate the public
//! API, but both sides must construct the same logical graph.

#![recursion_limit = "1024"]
#![type_length_limit = "16777216"]

use std::sync::Arc;

use criterion::{BatchSize, BenchmarkId, Criterion, black_box, criterion_group, criterion_main};
use hyphae::{
    CellImmutable, CellMap, MapQuery,
    traits::{LeftJoinExt, ProjectMapExt, SelectExt},
};

const MAX_DEPTH: usize = 12;
const DEFAULT_ROWS: usize = 1_000;

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
    rows: usize,
    buckets: usize,
}

fn populate(rows: usize) -> Sources {
    let buckets = (rows / 8).max(1);
    let root = CellMap::new();
    for i in 0..rows {
        root.insert(
            i as u64,
            Arc::new(Record {
                bucket: (i % buckets) as u64,
                checksum: i as u64,
                generation: 0,
            }),
        );
    }

    let dimensions = (0..MAX_DEPTH)
        .map(|source| {
            let map = CellMap::new();
            for i in 0..rows {
                map.insert(
                    i as u64,
                    Arc::new(Dimension {
                        bucket: (i % buckets) as u64,
                        payload: ((source + 1) as u64).wrapping_mul(10_000) + i as u64,
                    }),
                );
            }
            map
        })
        .collect();

    Sources {
        root,
        dimensions,
        rows,
        buckets,
    }
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
            });
    };
}

fn initial_plan(sources: &Sources) -> impl MapQuery<u64, Arc<Record>> + use<> {
    sources
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
        })
}

fn build_depth_4(sources: &Sources) -> CellMap<u64, Arc<Record>, CellImmutable> {
    let plan = initial_plan(sources);
    join_dimension!(plan, sources.dimensions[0], 1);
    join_dimension!(plan, sources.dimensions[1], 2);
    join_dimension!(plan, sources.dimensions[2], 3);
    join_dimension!(plan, sources.dimensions[3], 4);
    plan.materialize()
}

fn build_depth_8(sources: &Sources) -> CellMap<u64, Arc<Record>, CellImmutable> {
    let plan = initial_plan(sources);
    join_dimension!(plan, sources.dimensions[0], 1);
    join_dimension!(plan, sources.dimensions[1], 2);
    join_dimension!(plan, sources.dimensions[2], 3);
    join_dimension!(plan, sources.dimensions[3], 4);
    join_dimension!(plan, sources.dimensions[4], 5);
    join_dimension!(plan, sources.dimensions[5], 6);
    join_dimension!(plan, sources.dimensions[6], 7);
    join_dimension!(plan, sources.dimensions[7], 8);
    plan.materialize()
}

fn build_depth_12(sources: &Sources) -> CellMap<u64, Arc<Record>, CellImmutable> {
    let plan = initial_plan(sources);
    join_dimension!(plan, sources.dimensions[0], 1);
    join_dimension!(plan, sources.dimensions[1], 2);
    join_dimension!(plan, sources.dimensions[2], 3);
    join_dimension!(plan, sources.dimensions[3], 4);
    join_dimension!(plan, sources.dimensions[4], 5);
    join_dimension!(plan, sources.dimensions[5], 6);
    join_dimension!(plan, sources.dimensions[6], 7);
    join_dimension!(plan, sources.dimensions[7], 8);
    join_dimension!(plan, sources.dimensions[8], 9);
    join_dimension!(plan, sources.dimensions[9], 10);
    join_dimension!(plan, sources.dimensions[10], 11);
    join_dimension!(plan, sources.dimensions[11], 12);
    plan.materialize()
}

type Builder = fn(&Sources) -> CellMap<u64, Arc<Record>, CellImmutable>;

fn builder(depth: usize) -> Builder {
    match depth {
        4 => build_depth_4,
        8 => build_depth_8,
        12 => build_depth_12,
        _ => unreachable!("unsupported benchmark depth"),
    }
}

fn mutate_root(sources: &Sources, tick: u64) {
    let row = tick as usize % sources.rows;
    sources.root.insert(
        row as u64,
        Arc::new(Record {
            bucket: (row % sources.buckets) as u64,
            checksum: black_box(tick.wrapping_mul(17)),
            generation: tick,
        }),
    );
}

fn mutate_dimension(sources: &Sources, source: usize, tick: u64) {
    let row = tick as usize % sources.rows;
    sources.dimensions[source].insert(
        row as u64,
        Arc::new(Dimension {
            bucket: (row % sources.buckets) as u64,
            payload: black_box(tick.wrapping_mul(31).wrapping_add(source as u64)),
        }),
    );
}

fn bench_deep_root_updates(c: &mut Criterion) {
    let mut group = c.benchmark_group("reactive_graph/deep_root_update");
    group.sample_size(30);
    for depth in [4usize, 8, 12] {
        group.bench_with_input(BenchmarkId::from_parameter(depth), &depth, |b, &depth| {
            let sources = populate(DEFAULT_ROWS);
            let view = builder(depth)(&sources);
            let mut tick = 0u64;
            b.iter(|| {
                tick = tick.wrapping_add(1);
                mutate_root(&sources, tick);
                black_box(&view);
            });
        });
    }
    group.finish();
}

fn bench_deepest_source_updates(c: &mut Criterion) {
    let mut group = c.benchmark_group("reactive_graph/deepest_source_update");
    group.sample_size(30);
    for depth in [4usize, 8, 12] {
        group.bench_with_input(BenchmarkId::from_parameter(depth), &depth, |b, &depth| {
            let sources = populate(DEFAULT_ROWS);
            let view = builder(depth)(&sources);
            let mut tick = 0u64;
            b.iter(|| {
                tick = tick.wrapping_add(1);
                mutate_dimension(&sources, depth - 1, tick);
                black_box(&view);
            });
        });
    }
    group.finish();
}

fn bench_all_source_waves(c: &mut Criterion) {
    let mut group = c.benchmark_group("reactive_graph/all_source_wave");
    group.sample_size(20);
    for depth in [4usize, 8, 12] {
        group.bench_with_input(BenchmarkId::from_parameter(depth), &depth, |b, &depth| {
            let sources = populate(DEFAULT_ROWS);
            let view = builder(depth)(&sources);
            let mut tick = 0u64;
            b.iter(|| {
                tick = tick.wrapping_add(1);
                mutate_root(&sources, tick);
                for source in 0..depth {
                    mutate_dimension(&sources, source, tick.wrapping_add(source as u64));
                }
                black_box(&view);
            });
        });
    }
    group.finish();
}

fn bench_row_scaling(c: &mut Criterion) {
    let mut group = c.benchmark_group("reactive_graph/depth_12_row_scale");
    group.sample_size(20);
    for rows in [100usize, 1_000, 5_000] {
        group.bench_with_input(BenchmarkId::from_parameter(rows), &rows, |b, &rows| {
            let sources = populate(rows);
            let view = build_depth_12(&sources);
            let mut tick = 0u64;
            b.iter(|| {
                tick = tick.wrapping_add(1);
                mutate_root(&sources, tick);
                black_box(&view);
            });
        });
    }
    group.finish();
}

fn bench_materialize_and_teardown(c: &mut Criterion) {
    let mut group = c.benchmark_group("reactive_graph/materialize_and_teardown");
    group.sample_size(20);
    for depth in [4usize, 8, 12] {
        group.bench_with_input(BenchmarkId::from_parameter(depth), &depth, |b, &depth| {
            b.iter_batched(
                || populate(100),
                |sources| {
                    let view = builder(depth)(&sources);
                    black_box(&view);
                    drop(view);
                    drop(sources);
                },
                BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

fn bench_observer_fanout(c: &mut Criterion) {
    let mut group = c.benchmark_group("reactive_graph/depth_12_observer_fanout");
    group.sample_size(30);
    for observers in [1usize, 16, 128] {
        group.bench_with_input(
            BenchmarkId::from_parameter(observers),
            &observers,
            |b, &observers| {
                let sources = populate(DEFAULT_ROWS);
                let view = build_depth_12(&sources);
                let guards: Vec<_> = (0..observers)
                    .map(|_| {
                        view.subscribe_diffs(|diff| {
                            black_box(diff);
                        })
                    })
                    .collect();
                let mut tick = 0u64;
                b.iter(|| {
                    tick = tick.wrapping_add(1);
                    mutate_root(&sources, tick);
                });
                black_box(guards);
            },
        );
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_deep_root_updates,
    bench_deepest_source_updates,
    bench_all_source_waves,
    bench_row_scaling,
    bench_materialize_and_teardown,
    bench_observer_fanout,
);
criterion_main!(benches);
