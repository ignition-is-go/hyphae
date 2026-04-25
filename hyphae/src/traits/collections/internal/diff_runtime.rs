use std::{
    collections::{HashMap, HashSet},
    hash::Hash,
    sync::{Arc, Mutex},
};

use super::map_runtime::flatten_diff;
use crate::{
    cell_map::MapDiff,
    map_query::{MapDiffSink, MapQuery},
    subscription::SubscriptionGuard,
    traits::CellValue,
};

#[derive(Clone)]
struct GroupedState<SK, GK, GS>
where
    SK: Hash + Eq + CellValue,
    GK: Hash + Eq + CellValue,
    GS: Clone + Send + Sync + 'static,
{
    source_to_group: HashMap<SK, GK>,
    groups: HashMap<GK, GS>,
}

impl<SK, GK, GS> Default for GroupedState<SK, GK, GS>
where
    SK: Hash + Eq + CellValue,
    GK: Hash + Eq + CellValue,
    GS: Clone + Send + Sync + 'static,
{
    fn default() -> Self {
        Self {
            source_to_group: HashMap::new(),
            groups: HashMap::new(),
        }
    }
}

pub(crate) struct GroupedOps<SK, SV, GK, GS, OV, FG, FI, FU, FR, FM, FE>
where
    SK: Hash + Eq + CellValue,
    SV: CellValue,
    GK: Hash + Eq + CellValue,
    GS: Clone + Send + Sync + 'static,
    OV: CellValue,
    FG: Fn(&SK, &SV) -> GK + Send + Sync + 'static,
    FI: Fn(&mut GS, &SK, &SV) + Send + Sync + 'static,
    FU: Fn(&mut GS, &SK, &SV, &SV) + Send + Sync + 'static,
    FR: Fn(&mut GS, &SK, &SV) + Send + Sync + 'static,
    FM: Fn(&GS) -> OV + Send + Sync + 'static,
    FE: Fn(&GS) -> bool + Send + Sync + 'static,
{
    pub make_group_key: FG,
    pub on_insert: FI,
    pub on_update: FU,
    pub on_remove: FR,
    pub materialize: FM,
    pub is_empty: FE,
    pub new_group_state: fn() -> GS,
    pub _marker: std::marker::PhantomData<fn(&SK, &SV) -> GK>,
}

/// Wrap a non-empty change vector in `MapDiff::Batch`, dropping empty groups.
fn emit_changes<K, V>(changes: Vec<MapDiff<K, V>>, sink: &MapDiffSink<K, V>)
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    if changes.is_empty() {
        return;
    }
    sink(&MapDiff::Batch { changes });
}

/// Sink-driven diff runtime for [`MapQuery`] plan nodes.
///
/// Subscribes upstream by calling [`MapQuery::install`] on `source` with an
/// intermediate sink that flattens incoming diffs, drives `apply_atomic` to
/// build the downstream change set, and forwards a single `Batch` per
/// upstream emission to `sink`. No intermediate `CellMap` is allocated.
pub(crate) fn install_diff_runtime_via_query<SK, SV, OK, OV, S, ST, FS>(
    source: S,
    initial_state: ST,
    apply_atomic: FS,
    sink: MapDiffSink<OK, OV>,
) -> Vec<SubscriptionGuard>
where
    SK: Hash + Eq + CellValue,
    SV: CellValue,
    OK: Hash + Eq + CellValue,
    OV: CellValue,
    S: MapQuery<SK, SV>,
    ST: Send + Sync + 'static,
    FS: Fn(&mut ST, &MapDiff<SK, SV>, &mut Vec<MapDiff<OK, OV>>) + Send + Sync + 'static,
{
    let state = Arc::new(Mutex::new(initial_state));
    let apply_atomic = Arc::new(apply_atomic);

    let upstream_sink: MapDiffSink<SK, SV> = {
        let state = state.clone();
        let apply_atomic = apply_atomic.clone();
        let sink = sink.clone();
        Arc::new(move |diff| {
            let mut atomic_diffs: Vec<MapDiff<SK, SV>> = Vec::new();
            flatten_diff(diff, &mut atomic_diffs);
            let mut emitted = Vec::<MapDiff<OK, OV>>::new();
            {
                let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
                for atomic in &atomic_diffs {
                    apply_atomic(&mut state, atomic, &mut emitted);
                }
            }
            emit_changes(emitted, &sink);
        })
    };

    source.install(upstream_sink)
}

/// Sink-driven grouped runtime for [`MapQuery`] plan nodes.
///
/// Used by `CountByPlan` and `GroupByPlan` to install grouping machinery
/// into a downstream sink without materializing an intermediate `CellMap`.
pub(crate) fn install_grouped_runtime_via_query<
    SK,
    SV,
    GK,
    GS,
    OV,
    S,
    FG,
    FI,
    FU,
    FR,
    FM,
    FE,
>(
    source: S,
    ops: GroupedOps<SK, SV, GK, GS, OV, FG, FI, FU, FR, FM, FE>,
    sink: MapDiffSink<GK, OV>,
) -> Vec<SubscriptionGuard>
where
    SK: Hash + Eq + CellValue,
    SV: CellValue,
    GK: Hash + Eq + CellValue,
    GS: Clone + Send + Sync + 'static,
    OV: CellValue,
    S: MapQuery<SK, SV>,
    FG: Fn(&SK, &SV) -> GK + Send + Sync + 'static,
    FI: Fn(&mut GS, &SK, &SV) + Send + Sync + 'static,
    FU: Fn(&mut GS, &SK, &SV, &SV) + Send + Sync + 'static,
    FR: Fn(&mut GS, &SK, &SV) + Send + Sync + 'static,
    FM: Fn(&GS) -> OV + Send + Sync + 'static,
    FE: Fn(&GS) -> bool + Send + Sync + 'static,
{
    let ops = Arc::new(ops);
    install_diff_runtime_via_query::<SK, SV, GK, OV, S, GroupedState<SK, GK, GS>, _>(
        source,
        GroupedState::<SK, GK, GS>::default(),
        move |state: &mut GroupedState<SK, GK, GS>,
              diff: &MapDiff<SK, SV>,
              downstream: &mut Vec<MapDiff<GK, OV>>| {
            let mut touched = HashSet::<GK>::new();
            let mut before = HashMap::<GK, Option<OV>>::new();

            let capture_before = |group: &GK,
                                  state: &GroupedState<SK, GK, GS>,
                                  before: &mut HashMap<GK, Option<OV>>,
                                  touched: &mut HashSet<GK>| {
                before.entry(group.clone()).or_insert_with(|| {
                    state
                        .groups
                        .get(group)
                        .map(|group_state| (ops.materialize)(group_state))
                });
                touched.insert(group.clone());
            };

            match diff {
                MapDiff::Initial { entries } => {
                    let old_groups = std::mem::take(&mut state.groups);
                    state.source_to_group.clear();
                    for (group, group_state) in old_groups {
                        before.insert(group.clone(), Some((ops.materialize)(&group_state)));
                        touched.insert(group);
                    }
                    for (key, value) in entries {
                        let group = (ops.make_group_key)(key, value);
                        before.entry(group.clone()).or_insert(None);
                        touched.insert(group.clone());
                        state.source_to_group.insert(key.clone(), group.clone());
                        let group_state = state
                            .groups
                            .entry(group)
                            .or_insert_with(|| (ops.new_group_state)());
                        (ops.on_insert)(group_state, key, value);
                    }
                }
                MapDiff::Insert { key, value } => {
                    let group = (ops.make_group_key)(key, value);
                    capture_before(&group, state, &mut before, &mut touched);
                    state.source_to_group.insert(key.clone(), group.clone());
                    let group_state = state
                        .groups
                        .entry(group)
                        .or_insert_with(|| (ops.new_group_state)());
                    (ops.on_insert)(group_state, key, value);
                }
                MapDiff::Update {
                    key,
                    old_value,
                    new_value,
                } => {
                    let new_group = (ops.make_group_key)(key, new_value);
                    let old_group = state
                        .source_to_group
                        .insert(key.clone(), new_group.clone())
                        .unwrap_or_else(|| (ops.make_group_key)(key, old_value));
                    capture_before(&old_group, state, &mut before, &mut touched);
                    capture_before(&new_group, state, &mut before, &mut touched);

                    if old_group == new_group {
                        let group_state = state
                            .groups
                            .entry(new_group)
                            .or_insert_with(|| (ops.new_group_state)());
                        (ops.on_update)(group_state, key, old_value, new_value);
                    } else {
                        if let Some(group_state) = state.groups.get_mut(&old_group) {
                            (ops.on_remove)(group_state, key, old_value);
                            if (ops.is_empty)(group_state) {
                                state.groups.remove(&old_group);
                            }
                        }
                        let group_state = state
                            .groups
                            .entry(new_group)
                            .or_insert_with(|| (ops.new_group_state)());
                        (ops.on_insert)(group_state, key, new_value);
                    }
                }
                MapDiff::Remove { key, old_value } => {
                    let group = state
                        .source_to_group
                        .remove(key)
                        .unwrap_or_else(|| (ops.make_group_key)(key, old_value));
                    capture_before(&group, state, &mut before, &mut touched);
                    if let Some(group_state) = state.groups.get_mut(&group) {
                        (ops.on_remove)(group_state, key, old_value);
                        if (ops.is_empty)(group_state) {
                            state.groups.remove(&group);
                        }
                    }
                }
                MapDiff::Batch { .. } => {
                    unreachable!("install_diff_runtime_via_query flattens MapDiff::Batch")
                }
            }

            for group in touched {
                let old_value = before.get(&group).cloned().unwrap_or(None);
                let new_value = state
                    .groups
                    .get(&group)
                    .map(|group_state| (ops.materialize)(group_state));
                match (old_value, new_value) {
                    (Some(old_value), Some(new_value)) => downstream.push(MapDiff::Update {
                        key: group.clone(),
                        old_value,
                        new_value,
                    }),
                    (None, Some(new_value)) => downstream.push(MapDiff::Insert {
                        key: group.clone(),
                        value: new_value,
                    }),
                    (Some(old_value), None) => downstream.push(MapDiff::Remove {
                        key: group.clone(),
                        old_value,
                    }),
                    (None, None) => {}
                }
            }
        },
        sink,
    )
}
