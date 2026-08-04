//! Deep and wide application-shaped reactive graph benchmarks.
//!
//! This suite is intentionally dominated by `CellMap` query plans: every
//! materialized view joins up to 64 independently mutable data sources and
//! performs a projection after every join. It measures updates entering at the
//! root, updates entering at the deepest source, waves touching every source,
//! graph installation/teardown, row scaling, and observer fan-out.
//! A reported map depth is a `left_join_by` plus a `project` at every stage,
//! so depth 32 and 64 represent roughly 66 and 130 total query operators once
//! the initial select/project pair is included.
//!
//! Keep benchmark names and workloads stable across the pre/post migration
//! revisions. The `join_heavy_operator_pipeline` builders require explicit
//! intermediate `.materialize()` calls on v2.0.1 because its `join` operator
//! accepts cells, not pipelines; otherwise both sides construct the same
//! logical graph.

#![recursion_limit = "1024"]
#![type_length_limit = "16777216"]

use std::sync::Arc;

use criterion::{BatchSize, BenchmarkId, Criterion, black_box, criterion_group, criterion_main};
use hyphae::{
    Cell, CellImmutable, CellMap, JoinExt, MapExt, MapQuery, MaterializeDefinite,
    traits::{LeftJoinExt, ProjectMapExt, SelectExt},
};
use seq_macro::seq;

const MAX_DEPTH: usize = 64;
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

macro_rules! define_map_builder {
    ($name:ident, $depth:literal) => {
        fn $name(sources: &Sources) -> CellMap<u64, Arc<Record>, CellImmutable> {
            let plan = initial_plan(sources);
            seq!(N in 0..$depth {
                join_dimension!(plan, sources.dimensions[N], N as u64 + 1);
            });
            plan.materialize()
        }
    };
}

define_map_builder!(build_depth_4, 4);
define_map_builder!(build_depth_8, 8);
define_map_builder!(build_depth_12, 12);
define_map_builder!(build_depth_16, 16);
define_map_builder!(build_depth_32, 32);
define_map_builder!(build_depth_64, 64);

type Builder = fn(&Sources) -> CellMap<u64, Arc<Record>, CellImmutable>;

fn builder(depth: usize) -> Builder {
    match depth {
        4 => build_depth_4,
        8 => build_depth_8,
        12 => build_depth_12,
        16 => build_depth_16,
        32 => build_depth_32,
        64 => build_depth_64,
        _ => unreachable!("unsupported benchmark depth"),
    }
}

fn mutate_root(sources: &Sources, tick: u64) {
    let row = tick as usize % sources.rows;
    mutate_root_row(sources, row, tick);
}

fn mutate_root_row(sources: &Sources, row: usize, tick: u64) {
    sources.root.insert(
        row as u64,
        Arc::new(Record {
            bucket: (row % sources.buckets) as u64,
            checksum: black_box(tick.wrapping_mul(17)),
            generation: tick,
        }),
    );
}

fn record_value(record: &Option<Arc<Record>>) -> u64 {
    record
        .as_ref()
        .map_or(0, |record| record.checksum ^ record.generation)
}

fn sum_pair((left, right): &(u64, u64)) -> u64 {
    left.wrapping_add(*right).rotate_left(3)
}

macro_rules! join_view_value {
    ($plan:ident, $view:expr) => {
        let $plan = $plan
            .join(
                $view
                    .get(&0)
                    .map(record_value as fn(&Option<Arc<Record>>) -> u64),
            )
            .map(sum_pair as fn(&(u64, u64)) -> u64);
    };
}

macro_rules! define_operator_builder {
    ($name:ident, $depth:literal) => {
        fn $name(
            views: &[CellMap<u64, Arc<Record>, CellImmutable>],
        ) -> Cell<u64, CellImmutable> {
            let plan = views[0]
                .get(&0)
                .map(record_value as fn(&Option<Arc<Record>>) -> u64);
            seq!(N in 1..$depth {
                join_view_value!(plan, views[N]);
            });
            plan.materialize()
        }
    };
}

define_operator_builder!(operator_graph_depth_4, 4);
define_operator_builder!(operator_graph_depth_8, 8);
define_operator_builder!(operator_graph_depth_12, 12);
define_operator_builder!(operator_graph_depth_16, 16);
define_operator_builder!(operator_graph_depth_32, 32);
define_operator_builder!(operator_graph_depth_64, 64);

type OperatorBuilder = fn(&[CellMap<u64, Arc<Record>, CellImmutable>]) -> Cell<u64, CellImmutable>;

fn operator_builder(depth: usize) -> OperatorBuilder {
    match depth {
        4 => operator_graph_depth_4,
        8 => operator_graph_depth_8,
        12 => operator_graph_depth_12,
        16 => operator_graph_depth_16,
        32 => operator_graph_depth_32,
        64 => operator_graph_depth_64,
        _ => unreachable!("unsupported benchmark depth"),
    }
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
    for depth in [4usize, 8, 12, 16, 32, 64] {
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
    for depth in [4usize, 8, 12, 16, 32, 64] {
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
    for depth in [4usize, 8, 12, 16, 32, 64] {
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
    for depth in [4usize, 8, 12, 16, 32, 64] {
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

fn bench_join_heavy_operator_graph(c: &mut Criterion) {
    let mut group = c.benchmark_group("reactive_graph/join_heavy_operator_pipeline");
    group.sample_size(30);
    for depth in [4usize, 8, 12, 16, 32, 64] {
        group.bench_with_input(BenchmarkId::from_parameter(depth), &depth, |b, &depth| {
            let sources = populate(100);
            let views: Vec<_> = (0..depth).map(|_| build_depth_4(&sources)).collect();
            let output = operator_builder(depth)(&views);
            let mut tick = 0u64;
            b.iter(|| {
                tick = tick.wrapping_add(1);
                mutate_root_row(&sources, 0, tick);
                black_box(&output);
            });
        });
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
    bench_join_heavy_operator_graph,
);
criterion_main!(benches);
