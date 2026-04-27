//! Realistic application-shaped benchmarks.
//!
//! Mirrors the chain shapes, scale, and operator mix observed in production
//! reactive applications: derived views composed of multiple joins +
//! projections + group_by over event-driven entity tables. Patterns:
//!
//! - **mid_view (4-hop)**: `instances.left_join_by(&lanes).left_join_by(&keyframes).project()`
//! - **assets_view (5-hop)**: `files.group_by().left_join_by(transfers).project().left_join_by(metadata).project()`
//! - **deep_view (7-hop)**: `targets.left_join_by(actions).project().left_join_by(emitters).project().left_join_by(statuses).project()`
//! - **fan_out**: a mid_view materialized once, then read by N subscribers
//! - **batch_mutation**: insert_many of K rows through a mid_view
//! - **select_project**: `select` (filter) followed by `project` (reshape)
//!
//! Entity values use `Arc<str>` keys + `Arc<EntityStruct>` values to match the
//! lightweight-Arc-handle data shape that real applications use.
//!
//! Scales: 100, 1k, 10k rows in source maps.

use std::sync::Arc;

use criterion::{BenchmarkId, Criterion, black_box, criterion_group, criterion_main};
use hyphae::{
    CellMap, MapQuery,
    traits::{GroupByExt, LeftJoinExt, ProjectMapExt, SelectExt},
};

// ── Entity types ─────────────────────────────────────────────────────────────

#[derive(Clone, PartialEq, Debug)]
struct Instance {
    id: Arc<str>,
    lane_id: Arc<str>,
    track_id: Arc<str>,
}

#[derive(Clone, PartialEq, Debug)]
struct Lane {
    id: Arc<str>,
    name: Arc<str>,
}

#[derive(Clone, PartialEq, Debug)]
struct Keyframe {
    id: Arc<str>,
    lane_id: Arc<str>,
    value: f64,
}

#[derive(Clone, PartialEq, Debug)]
struct File {
    id: Arc<str>,
    asset_path: Arc<str>,
    size: u64,
}

#[derive(Clone, PartialEq, Debug)]
struct Transfer {
    id: Arc<str>,
    asset_path: Arc<str>,
    progress: f32,
}

#[derive(Clone, PartialEq, Debug)]
struct Metadata {
    id: Arc<str>,
    asset_path: Arc<str>,
    tag: Arc<str>,
}

#[derive(Clone, PartialEq, Debug)]
struct Target {
    id: Arc<str>,
    parent_id: Option<Arc<str>>,
    name: Arc<str>,
}

#[derive(Clone, PartialEq, Debug)]
struct Action {
    id: Arc<str>,
    target_id: Arc<str>,
    name: Arc<str>,
}

#[derive(Clone, PartialEq, Debug)]
struct Emitter {
    id: Arc<str>,
    target_id: Arc<str>,
    rate: f32,
}

#[derive(Clone, PartialEq, Debug)]
struct Status {
    id: Arc<str>,
    target_id: Arc<str>,
    online: bool,
}

// ── Output view items ────────────────────────────────────────────────────────

#[derive(Clone, PartialEq, Debug)]
struct ValueTrackEditorLane {
    id: Arc<str>,
    instance: Arc<Instance>,
    lane: Arc<Lane>,
    keyframes: Vec<Arc<Keyframe>>,
}

#[derive(Clone, PartialEq, Debug)]
struct AssetView {
    asset_path: Arc<str>,
    files: Vec<Arc<File>>,
    transfers: Vec<Arc<Transfer>>,
    metadata: Vec<Arc<Metadata>>,
}

#[derive(Clone, PartialEq, Debug)]
struct SessionTargetView {
    id: Arc<str>,
    target: Arc<Target>,
    actions: Vec<Arc<Action>>,
    emitters: Vec<Arc<Emitter>>,
    statuses: Vec<Arc<Status>>,
}

// ── Scale helpers ────────────────────────────────────────────────────────────

fn s(prefix: &str, i: usize) -> Arc<str> {
    Arc::from(format!("{prefix}{i}").as_str())
}

/// Build N instances pointing to N/4 distinct lanes (so each lane is referenced
/// by ~4 instances on average — roughly the rship pattern).
fn populate_value_track_sources(n: usize) -> (CellMap<Arc<str>, Arc<Instance>>, CellMap<Arc<str>, Arc<Lane>>, CellMap<Arc<str>, Arc<Keyframe>>) {
    let instances = CellMap::<Arc<str>, Arc<Instance>>::new();
    let lanes = CellMap::<Arc<str>, Arc<Lane>>::new();
    let keyframes = CellMap::<Arc<str>, Arc<Keyframe>>::new();

    let lane_count = (n / 4).max(1);

    for i in 0..lane_count {
        let id = s("lane:", i);
        lanes.insert(
            id.clone(),
            Arc::new(Lane {
                id: id.clone(),
                name: s("Lane ", i),
            }),
        );
    }

    for i in 0..n {
        let lane_id = s("lane:", i % lane_count);
        let id = s("inst:", i);
        instances.insert(
            id.clone(),
            Arc::new(Instance {
                id: id.clone(),
                lane_id: lane_id.clone(),
                track_id: s("track:", i % 16),
            }),
        );
    }

    // ~3 keyframes per lane on average.
    for i in 0..(lane_count * 3) {
        let lane_id = s("lane:", i % lane_count);
        let id = s("kf:", i);
        keyframes.insert(
            id.clone(),
            Arc::new(Keyframe {
                id: id.clone(),
                lane_id,
                value: i as f64 * 0.5,
            }),
        );
    }

    (instances, lanes, keyframes)
}

fn populate_assets_sources(
    n: usize,
) -> (
    CellMap<Arc<str>, Arc<File>>,
    CellMap<Arc<str>, Arc<Transfer>>,
    CellMap<Arc<str>, Arc<Metadata>>,
) {
    let files = CellMap::<Arc<str>, Arc<File>>::new();
    let transfers = CellMap::<Arc<str>, Arc<Transfer>>::new();
    let metadata = CellMap::<Arc<str>, Arc<Metadata>>::new();

    // ~4 files per asset_path; ~1 transfer per asset_path; ~2 metadata entries.
    let asset_count = (n / 4).max(1);

    for i in 0..n {
        let asset_path = s("asset:", i % asset_count);
        let id = s("file:", i);
        files.insert(
            id.clone(),
            Arc::new(File {
                id: id.clone(),
                asset_path,
                size: 1024 * (i as u64 + 1),
            }),
        );
    }
    for i in 0..asset_count {
        let id = s("xfer:", i);
        transfers.insert(
            id.clone(),
            Arc::new(Transfer {
                id: id.clone(),
                asset_path: s("asset:", i),
                progress: (i % 100) as f32 / 100.0,
            }),
        );
    }
    for i in 0..(asset_count * 2) {
        let id = s("meta:", i);
        metadata.insert(
            id.clone(),
            Arc::new(Metadata {
                id: id.clone(),
                asset_path: s("asset:", i % asset_count),
                tag: s("tag:", i % 8),
            }),
        );
    }

    (files, transfers, metadata)
}

fn populate_session_sources(
    n_targets: usize,
) -> (
    CellMap<Arc<str>, Arc<Target>>,
    CellMap<Arc<str>, Arc<Action>>,
    CellMap<Arc<str>, Arc<Emitter>>,
    CellMap<Arc<str>, Arc<Status>>,
) {
    let targets = CellMap::<Arc<str>, Arc<Target>>::new();
    let actions = CellMap::<Arc<str>, Arc<Action>>::new();
    let emitters = CellMap::<Arc<str>, Arc<Emitter>>::new();
    let statuses = CellMap::<Arc<str>, Arc<Status>>::new();

    for i in 0..n_targets {
        let id = s("tgt:", i);
        targets.insert(
            id.clone(),
            Arc::new(Target {
                id: id.clone(),
                parent_id: if i > 0 { Some(s("tgt:", i / 4)) } else { None },
                name: s("Target ", i),
            }),
        );
    }
    // ~3 actions per target.
    for i in 0..(n_targets * 3) {
        let id = s("act:", i);
        actions.insert(
            id.clone(),
            Arc::new(Action {
                id: id.clone(),
                target_id: s("tgt:", i % n_targets),
                name: s("act ", i),
            }),
        );
    }
    // ~2 emitters per target.
    for i in 0..(n_targets * 2) {
        let id = s("emit:", i);
        emitters.insert(
            id.clone(),
            Arc::new(Emitter {
                id: id.clone(),
                target_id: s("tgt:", i % n_targets),
                rate: 60.0,
            }),
        );
    }
    // ~1 status per target.
    for i in 0..n_targets {
        let id = s("stat:", i);
        statuses.insert(
            id.clone(),
            Arc::new(Status {
                id: id.clone(),
                target_id: s("tgt:", i),
                online: i % 2 == 0,
            }),
        );
    }

    (targets, actions, emitters, statuses)
}

// ── Scenario 1: mid_view (4-hop, ValueTrackEditor-like) ──────────────────────
//
//   instances.left_join_by(&lanes).left_join_by(&keyframes).project()

fn build_mid_view(
    instances: &CellMap<Arc<str>, Arc<Instance>>,
    lanes: &CellMap<Arc<str>, Arc<Lane>>,
    keyframes: &CellMap<Arc<str>, Arc<Keyframe>>,
) -> CellMap<Arc<str>, Arc<ValueTrackEditorLane>, hyphae::CellImmutable> {
    instances
        .clone()
        .left_join_by(
            lanes.clone(),
            |_inst_id, inst| inst.lane_id.clone(),
            |lane_id, _lane| lane_id.clone(),
        )
        .left_join_by(
            keyframes.clone(),
            |_inst_id, (inst, _lanes)| inst.lane_id.clone(),
            |_kf_id, kf| kf.lane_id.clone(),
        )
        .project(|inst_id, ((inst, lane_matches), kf_list)| {
            let lane = lane_matches.first()?.clone();
            Some((
                inst_id.clone(),
                Arc::new(ValueTrackEditorLane {
                    id: inst_id.clone(),
                    instance: inst.clone(),
                    lane,
                    keyframes: kf_list.clone(),
                }),
            ))
        })
        .materialize()
}

fn bench_mid_view(c: &mut Criterion) {
    let mut group = c.benchmark_group("view/mid_view_4hop");
    group.sample_size(50);
    for n in [100usize, 1_000, 10_000] {
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
            let (instances, lanes, keyframes) = populate_value_track_sources(n);
            let _view = build_mid_view(&instances, &lanes, &keyframes);
            let mut tick = 0u64;
            b.iter(|| {
                tick = tick.wrapping_add(1);
                let id = s("inst:", (tick as usize) % n);
                instances.insert(
                    id.clone(),
                    Arc::new(Instance {
                        id,
                        lane_id: s("lane:", (tick as usize) % (n / 4).max(1)),
                        track_id: s("track:", black_box(tick) as usize % 16),
                    }),
                );
            });
        });
    }
    group.finish();
}

// ── Scenario 2: assets_view (5-hop, AssetsView-like with group_by) ───────────
//
//   files.group_by(asset_path)
//        .left_join_by(transfers, ...).project(...)
//        .left_join_by(metadata, ...).project(...)

fn build_assets_view(
    files: &CellMap<Arc<str>, Arc<File>>,
    transfers: &CellMap<Arc<str>, Arc<Transfer>>,
    metadata: &CellMap<Arc<str>, Arc<Metadata>>,
) -> CellMap<Arc<str>, Arc<AssetView>, hyphae::CellImmutable> {
    let by_asset = files
        .clone()
        .group_by(|_id, f| f.asset_path.clone())
        .materialize();

    by_asset
        .clone()
        .left_join_by(
            transfers.clone(),
            |asset_path, _files| asset_path.clone(),
            |_id, t| t.asset_path.clone(),
        )
        .project(|asset_path, (files, transfers)| {
            Some((
                asset_path.clone(),
                (files.clone(), transfers.clone()),
            ))
        })
        .left_join_by(
            metadata.clone(),
            |asset_path, _v| asset_path.clone(),
            |_id, m| m.asset_path.clone(),
        )
        .project(|asset_path, ((files, transfers), metadata)| {
            Some((
                asset_path.clone(),
                Arc::new(AssetView {
                    asset_path: asset_path.clone(),
                    files: files.clone(),
                    transfers: transfers.clone(),
                    metadata: metadata.clone(),
                }),
            ))
        })
        .materialize()
}

fn bench_assets_view(c: &mut Criterion) {
    let mut group = c.benchmark_group("view/assets_view_5hop");
    group.sample_size(50);
    for n in [100usize, 1_000, 10_000] {
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
            let (files, transfers, metadata) = populate_assets_sources(n);
            let _view = build_assets_view(&files, &transfers, &metadata);
            let mut tick = 0u64;
            b.iter(|| {
                tick = tick.wrapping_add(1);
                let id = s("file:", (tick as usize) % n);
                files.insert(
                    id.clone(),
                    Arc::new(File {
                        id,
                        asset_path: s("asset:", (tick as usize) % (n / 4).max(1)),
                        size: black_box(tick),
                    }),
                );
            });
        });
    }
    group.finish();
}

// ── Scenario 3: deep_view (7-hop, SessionTargets-like) ───────────────────────
//
//   targets.left_join_by(actions).project()
//          .left_join_by(emitters).project()
//          .left_join_by(statuses).project()

fn build_session_view(
    targets: &CellMap<Arc<str>, Arc<Target>>,
    actions: &CellMap<Arc<str>, Arc<Action>>,
    emitters: &CellMap<Arc<str>, Arc<Emitter>>,
    statuses: &CellMap<Arc<str>, Arc<Status>>,
) -> CellMap<Arc<str>, Arc<SessionTargetView>, hyphae::CellImmutable> {
    targets
        .clone()
        .left_join_by(
            actions.clone(),
            |id, _t| id.clone(),
            |_id, a| a.target_id.clone(),
        )
        .project(|tgt_id, (target, action_list)| {
            Some((
                tgt_id.clone(),
                (target.clone(), action_list.clone()),
            ))
        })
        .left_join_by(
            emitters.clone(),
            |id, _v| id.clone(),
            |_id, e| e.target_id.clone(),
        )
        .project(|tgt_id, ((target, actions), emitter_list)| {
            Some((
                tgt_id.clone(),
                (target.clone(), actions.clone(), emitter_list.clone()),
            ))
        })
        .left_join_by(
            statuses.clone(),
            |id, _v| id.clone(),
            |_id, s| s.target_id.clone(),
        )
        .project(|tgt_id, ((target, actions, emitters), status_list)| {
            Some((
                tgt_id.clone(),
                Arc::new(SessionTargetView {
                    id: tgt_id.clone(),
                    target: target.clone(),
                    actions: actions.clone(),
                    emitters: emitters.clone(),
                    statuses: status_list.clone(),
                }),
            ))
        })
        .materialize()
}

fn bench_deep_view(c: &mut Criterion) {
    let mut group = c.benchmark_group("view/deep_view_7hop");
    group.sample_size(30);
    for n in [100usize, 1_000, 5_000] {
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
            let (targets, actions, emitters, statuses) = populate_session_sources(n);
            let _view = build_session_view(&targets, &actions, &emitters, &statuses);
            let mut tick = 0u64;
            b.iter(|| {
                tick = tick.wrapping_add(1);
                // Mutate an action — exercises the deepest join's right-side update path.
                let id = s("act:", (tick as usize) % (n * 3));
                actions.insert(
                    id.clone(),
                    Arc::new(Action {
                        id,
                        target_id: s("tgt:", (tick as usize) % n),
                        name: s("act ", black_box(tick) as usize),
                    }),
                );
            });
        });
    }
    group.finish();
}

// ── Scenario 4: fan_out (mid_view + N subscribers reading entries) ───────────

fn bench_fan_out(c: &mut Criterion) {
    let mut group = c.benchmark_group("view/fan_out_mid_view");
    group.sample_size(50);
    let n = 1_000usize;
    for n_subs in [1usize, 5, 20, 100] {
        group.bench_with_input(BenchmarkId::from_parameter(n_subs), &n_subs, |b, &n_subs| {
            let (instances, lanes, keyframes) = populate_value_track_sources(n);
            let view = build_mid_view(&instances, &lanes, &keyframes);

            // Attach N independent diff subscribers to the materialized view's
            // diff stream — models the fan-out where N consumers observe the same
            // derived view.
            let _guards: Vec<_> = (0..n_subs)
                .map(|_| {
                    view.subscribe_diffs(|diff| {
                        // No-op subscriber, but still pays the dispatch cost.
                        black_box(diff);
                    })
                })
                .collect();

            let mut tick = 0u64;
            b.iter(|| {
                tick = tick.wrapping_add(1);
                let id = s("inst:", (tick as usize) % n);
                instances.insert(
                    id.clone(),
                    Arc::new(Instance {
                        id,
                        lane_id: s("lane:", (tick as usize) % (n / 4).max(1)),
                        track_id: s("track:", black_box(tick) as usize % 16),
                    }),
                );
            });
        });
    }
    group.finish();
}

// ── Scenario 5: batch_mutation (apply_batch through a mid_view) ──────────────

fn bench_batch_mutation(c: &mut Criterion) {
    let mut group = c.benchmark_group("view/batch_mutation_mid_view");
    group.sample_size(50);
    let n = 1_000usize;
    for batch_size in [1usize, 10, 100] {
        group.bench_with_input(
            BenchmarkId::from_parameter(batch_size),
            &batch_size,
            |b, &batch_size| {
                let (instances, lanes, keyframes) = populate_value_track_sources(n);
                let _view = build_mid_view(&instances, &lanes, &keyframes);

                let mut tick = 0u64;
                b.iter(|| {
                    let entries: Vec<(Arc<str>, Arc<Instance>)> = (0..batch_size)
                        .map(|i| {
                            tick = tick.wrapping_add(1);
                            let id = s("inst:", (tick as usize) % n);
                            (
                                id.clone(),
                                Arc::new(Instance {
                                    id,
                                    lane_id: s("lane:", (tick as usize) % (n / 4).max(1)),
                                    track_id: s(
                                        "track:",
                                        black_box(i + tick as usize) % 16,
                                    ),
                                }),
                            )
                        })
                        .collect();
                    instances.insert_many(entries);
                });
            },
        );
    }
    group.finish();
}

// ── Scenario 6: select-then-project (filtering + reshape, very common) ───────
//
//   targets.select(|t| t.parent_id.is_some()).project(...)

fn bench_select_project(c: &mut Criterion) {
    let mut group = c.benchmark_group("view/select_project");
    group.sample_size(50);
    for n in [100usize, 1_000, 10_000] {
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, &n| {
            let (targets, _, _, _) = populate_session_sources(n);
            let _view = targets
                .clone()
                .select(|t| t.parent_id.is_some())
                .project(|id, t| Some((id.clone(), t.name.clone())))
                .materialize();
            let mut tick = 0u64;
            b.iter(|| {
                tick = tick.wrapping_add(1);
                let i = (tick as usize) % n;
                let id = s("tgt:", i);
                targets.insert(
                    id.clone(),
                    Arc::new(Target {
                        id,
                        parent_id: if i > 0 { Some(s("tgt:", i / 4)) } else { None },
                        name: s("Target ", black_box(tick) as usize),
                    }),
                );
            });
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_mid_view,
    bench_assets_view,
    bench_deep_view,
    bench_fan_out,
    bench_batch_mutation,
    bench_select_project,
);
criterion_main!(benches);
