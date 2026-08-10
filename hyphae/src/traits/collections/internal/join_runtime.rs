use std::{
    collections::hash_map::Entry,
    hash::Hash,
    sync::{Arc, Mutex},
};

use rustc_hash::FxHashMap;

use crate::{cell_map::MapDiff, subscription::SubscriptionGuard, traits::CellValue};

use super::ordered_set::OrderedSet;

struct JoinState<LK, LV, RK, RV, JK, OK, OV>
where
    LK: Hash + Eq + CellValue,
    LV: CellValue,
    RK: Hash + Eq + CellValue,
    RV: CellValue,
    JK: Hash + Eq + CellValue,
    OK: Hash + Eq + CellValue,
    OV: CellValue,
{
    left_rows: FxHashMap<LK, LV>,
    left_join_keys: FxHashMap<LK, JK>,
    join_to_left: FxHashMap<JK, Vec<LK>>,
    right_rows: FxHashMap<RK, RV>,
    right_join_keys: FxHashMap<RK, JK>,
    join_to_right: FxHashMap<JK, Vec<RK>>,
    left_output_keys: FxHashMap<LK, OrderedSet<OK>>,
    output_cache: FxHashMap<OK, OV>,
    parallel_active: bool,
    scratch: JoinScratch<LK, RK, RV, JK, OK, OV>,
}

struct JoinScratch<LK, RK, RV, JK, OK, OV> {
    impacted: OrderedSet<LK>,
    changed_join_keys: OrderedSet<JK>,
    right_rows: Vec<(RK, RV)>,
    desired_rows: FxHashMap<OK, OV>,
    desired_order: OrderedSet<OK>,
}

impl<LK, RK, RV, JK, OK, OV> Default for JoinScratch<LK, RK, RV, JK, OK, OV> {
    fn default() -> Self {
        Self {
            impacted: OrderedSet::default(),
            changed_join_keys: OrderedSet::default(),
            right_rows: Vec::new(),
            desired_rows: FxHashMap::default(),
            desired_order: OrderedSet::default(),
        }
    }
}

impl<LK, LV, RK, RV, JK, OK, OV> Default for JoinState<LK, LV, RK, RV, JK, OK, OV>
where
    LK: Hash + Eq + CellValue,
    LV: CellValue,
    RK: Hash + Eq + CellValue,
    RV: CellValue,
    JK: Hash + Eq + CellValue,
    OK: Hash + Eq + CellValue,
    OV: CellValue,
{
    fn default() -> Self {
        Self {
            left_rows: FxHashMap::default(),
            left_join_keys: FxHashMap::default(),
            join_to_left: FxHashMap::default(),
            right_rows: FxHashMap::default(),
            right_join_keys: FxHashMap::default(),
            join_to_right: FxHashMap::default(),
            left_output_keys: FxHashMap::default(),
            output_cache: FxHashMap::default(),
            parallel_active: false,
            scratch: JoinScratch::default(),
        }
    }
}

fn add_index_member<I, M>(index: &mut FxHashMap<I, Vec<M>>, index_key: I, member: M)
where
    I: Hash + Eq + CellValue,
    M: Hash + Eq + CellValue,
{
    let members = index.entry(index_key).or_default();
    if !members.contains(&member) {
        members.push(member);
    }
}

fn remove_index_member<I, M>(index: &mut FxHashMap<I, Vec<M>>, index_key: &I, member: &M)
where
    I: Hash + Eq + CellValue,
    M: Hash + Eq + CellValue,
{
    if let Some(members) = index.get_mut(index_key) {
        members.retain(|candidate| candidate != member);
        if members.is_empty() {
            index.remove(index_key);
        }
    }
}

fn upsert_left<LK, LV, RK, RV, JK, OK, OV, FL>(
    state: &mut JoinState<LK, LV, RK, RV, JK, OK, OV>,
    left_key: LK,
    left_value: LV,
    left_join_key: &FL,
    impacted: &mut OrderedSet<LK>,
) where
    LK: Hash + Eq + CellValue,
    LV: CellValue,
    RK: Hash + Eq + CellValue,
    RV: CellValue,
    JK: Hash + Eq + CellValue,
    OK: Hash + Eq + CellValue,
    OV: CellValue,
    FL: Fn(&LK, &LV) -> JK,
{
    let join_key = left_join_key(&left_key, &left_value);
    match state
        .left_join_keys
        .insert(left_key.clone(), join_key.clone())
    {
        Some(previous_join_key) if previous_join_key != join_key => {
            remove_index_member(&mut state.join_to_left, &previous_join_key, &left_key);
            add_index_member(&mut state.join_to_left, join_key, left_key.clone());
        }
        Some(_) => {}
        None => add_index_member(&mut state.join_to_left, join_key, left_key.clone()),
    }
    state.left_rows.insert(left_key.clone(), left_value);
    impacted.insert(left_key);
}

fn remove_left<LK, LV, RK, RV, JK, OK, OV>(
    state: &mut JoinState<LK, LV, RK, RV, JK, OK, OV>,
    left_key: &LK,
    impacted: &mut OrderedSet<LK>,
) where
    LK: Hash + Eq + CellValue,
    LV: CellValue,
    RK: Hash + Eq + CellValue,
    RV: CellValue,
    JK: Hash + Eq + CellValue,
    OK: Hash + Eq + CellValue,
    OV: CellValue,
{
    if let Some(previous_join_key) = state.left_join_keys.remove(left_key) {
        remove_index_member(&mut state.join_to_left, &previous_join_key, left_key);
    }
    if state.left_rows.remove(left_key).is_some() || state.left_output_keys.contains_key(left_key) {
        impacted.insert(left_key.clone());
    }
}

fn apply_left_diff<LK, LV, RK, RV, JK, OK, OV, FL>(
    state: &mut JoinState<LK, LV, RK, RV, JK, OK, OV>,
    diff: &MapDiff<LK, LV>,
    left_join_key: &FL,
    impacted: &mut OrderedSet<LK>,
) where
    LK: Hash + Eq + CellValue,
    LV: CellValue,
    RK: Hash + Eq + CellValue,
    RV: CellValue,
    JK: Hash + Eq + CellValue,
    OK: Hash + Eq + CellValue,
    OV: CellValue,
    FL: Fn(&LK, &LV) -> JK,
{
    match diff {
        MapDiff::Initial { entries } => {
            let previous_left_keys: Vec<LK> = state.left_rows.keys().cloned().collect();
            state.left_rows.clear();
            state.left_join_keys.clear();
            state.join_to_left.clear();
            for key in previous_left_keys {
                impacted.insert(key);
            }
            for (key, value) in entries {
                upsert_left(state, key.clone(), value.clone(), left_join_key, impacted);
            }
        }
        MapDiff::Insert { key, value }
        | MapDiff::Update {
            key,
            new_value: value,
            ..
        } => {
            upsert_left(state, key.clone(), value.clone(), left_join_key, impacted);
        }
        MapDiff::Remove { key, .. } => {
            remove_left(state, key, impacted);
        }
        MapDiff::Batch { changes } => {
            for change in changes {
                apply_left_diff(state, change, left_join_key, impacted);
            }
        }
    }
}

fn upsert_right<LK, LV, RK, RV, JK, OK, OV, FR>(
    state: &mut JoinState<LK, LV, RK, RV, JK, OK, OV>,
    right_key: RK,
    right_value: RV,
    right_join_key: &FR,
    changed_join_keys: &mut OrderedSet<JK>,
) where
    LK: Hash + Eq + CellValue,
    LV: CellValue,
    RK: Hash + Eq + CellValue,
    RV: CellValue,
    JK: Hash + Eq + CellValue,
    OK: Hash + Eq + CellValue,
    OV: CellValue,
    FR: Fn(&RK, &RV) -> JK,
{
    let join_key = right_join_key(&right_key, &right_value);
    match state
        .right_join_keys
        .insert(right_key.clone(), join_key.clone())
    {
        Some(previous_join_key) if previous_join_key != join_key => {
            remove_index_member(&mut state.join_to_right, &previous_join_key, &right_key);
            changed_join_keys.insert(previous_join_key);
            add_index_member(
                &mut state.join_to_right,
                join_key.clone(),
                right_key.clone(),
            );
        }
        Some(_) => {}
        None => add_index_member(
            &mut state.join_to_right,
            join_key.clone(),
            right_key.clone(),
        ),
    }
    state.right_rows.insert(right_key, right_value);
    changed_join_keys.insert(join_key);
}

fn remove_right<LK, LV, RK, RV, JK, OK, OV>(
    state: &mut JoinState<LK, LV, RK, RV, JK, OK, OV>,
    right_key: &RK,
    changed_join_keys: &mut OrderedSet<JK>,
) where
    LK: Hash + Eq + CellValue,
    LV: CellValue,
    RK: Hash + Eq + CellValue,
    RV: CellValue,
    JK: Hash + Eq + CellValue,
    OK: Hash + Eq + CellValue,
    OV: CellValue,
{
    if let Some(previous_join_key) = state.right_join_keys.remove(right_key) {
        remove_index_member(&mut state.join_to_right, &previous_join_key, right_key);
        changed_join_keys.insert(previous_join_key);
    }
    state.right_rows.remove(right_key);
}

fn apply_right_diff<LK, LV, RK, RV, JK, OK, OV, FR>(
    state: &mut JoinState<LK, LV, RK, RV, JK, OK, OV>,
    diff: &MapDiff<RK, RV>,
    right_join_key: &FR,
    impacted: &mut OrderedSet<LK>,
    changed_join_keys: &mut OrderedSet<JK>,
) where
    LK: Hash + Eq + CellValue,
    LV: CellValue,
    RK: Hash + Eq + CellValue,
    RV: CellValue,
    JK: Hash + Eq + CellValue,
    OK: Hash + Eq + CellValue,
    OV: CellValue,
    FR: Fn(&RK, &RV) -> JK,
{
    fn apply_one<LK, LV, RK, RV, JK, OK, OV, FR>(
        state: &mut JoinState<LK, LV, RK, RV, JK, OK, OV>,
        diff: &MapDiff<RK, RV>,
        right_join_key: &FR,
        changed_join_keys: &mut OrderedSet<JK>,
    ) where
        LK: Hash + Eq + CellValue,
        LV: CellValue,
        RK: Hash + Eq + CellValue,
        RV: CellValue,
        JK: Hash + Eq + CellValue,
        OK: Hash + Eq + CellValue,
        OV: CellValue,
        FR: Fn(&RK, &RV) -> JK,
    {
        match diff {
            MapDiff::Initial { entries } => {
                for join_key in state.right_join_keys.values() {
                    changed_join_keys.insert(join_key.clone());
                }
                state.right_rows.clear();
                state.right_join_keys.clear();
                state.join_to_right.clear();
                for (key, value) in entries {
                    upsert_right(
                        state,
                        key.clone(),
                        value.clone(),
                        right_join_key,
                        changed_join_keys,
                    );
                }
            }
            MapDiff::Insert { key, value }
            | MapDiff::Update {
                key,
                new_value: value,
                ..
            } => {
                upsert_right(
                    state,
                    key.clone(),
                    value.clone(),
                    right_join_key,
                    changed_join_keys,
                );
            }
            MapDiff::Remove { key, .. } => {
                remove_right(state, key, changed_join_keys);
            }
            MapDiff::Batch { changes } => {
                for change in changes {
                    apply_one(state, change, right_join_key, changed_join_keys);
                }
            }
        }
    }

    apply_one(state, diff, right_join_key, changed_join_keys);

    for join_key in changed_join_keys.drain() {
        if let Some(left_keys) = state.join_to_left.get(&join_key) {
            impacted.extend(left_keys.iter().cloned());
        }
    }
}

fn recompute_impacted<LK, LV, RK, RV, JK, OK, OV, FO>(
    state: &mut JoinState<LK, LV, RK, RV, JK, OK, OV>,
    scratch: &mut JoinScratch<LK, RK, RV, JK, OK, OV>,
    compute_rows: &FO,
) -> Vec<MapDiff<OK, OV>>
where
    LK: Hash + Eq + CellValue,
    LV: CellValue,
    RK: Hash + Eq + CellValue,
    RV: CellValue,
    JK: Hash + Eq + CellValue,
    OK: Hash + Eq + CellValue,
    OV: CellValue,
    FO: Fn(&LK, &LV, &[(RK, RV)]) -> Vec<(OK, OV)>,
{
    let mut changes: Vec<MapDiff<OK, OV>> = Vec::new();

    for left_key in scratch.impacted.drain() {
        let mut current_output_keys = state.left_output_keys.remove(&left_key).unwrap_or_default();

        scratch.desired_rows.clear();
        scratch.desired_order.clear();
        if let Some(left_value) = state.left_rows.get(&left_key) {
            scratch.right_rows.clear();
            if let Some(right_keys) = state
                .left_join_keys
                .get(&left_key)
                .and_then(|join_key| state.join_to_right.get(join_key))
            {
                scratch
                    .right_rows
                    .extend(right_keys.iter().filter_map(|right_key| {
                        state
                            .right_rows
                            .get(right_key)
                            .map(|right_value| (right_key.clone(), right_value.clone()))
                    }));
            }

            for (output_key, output_value) in
                compute_rows(&left_key, left_value, &scratch.right_rows)
            {
                scratch.desired_order.insert(output_key.clone());
                scratch.desired_rows.insert(output_key, output_value);
            }
        }

        current_output_keys.retain(|output_key| {
            if scratch.desired_rows.contains_key(output_key) {
                true
            } else {
                if let Some(old_value) = state.output_cache.remove(output_key) {
                    changes.push(MapDiff::Remove {
                        key: output_key.clone(),
                        old_value,
                    });
                }
                false
            }
        });

        for output_key in scratch.desired_order.drain() {
            let Some(new_value) = scratch.desired_rows.remove(&output_key) else {
                continue;
            };
            current_output_keys.insert(output_key.clone());
            match state.output_cache.entry(output_key.clone()) {
                Entry::Occupied(mut entry) => {
                    if entry.get() != &new_value {
                        let old_value = entry.insert(new_value.clone());
                        changes.push(MapDiff::Update {
                            key: output_key,
                            old_value,
                            new_value,
                        });
                    }
                }
                Entry::Vacant(entry) => {
                    entry.insert(new_value.clone());
                    changes.push(MapDiff::Insert {
                        key: output_key,
                        value: new_value,
                    });
                }
            }
        }

        if !current_output_keys.is_empty() {
            state.left_output_keys.insert(left_key, current_output_keys);
        }
    }

    changes
}

/// Recompute joins that produce zero or one output under the unchanged left key.
///
/// This covers left joins, semi joins, and key-equal inner joins. Their output
/// shape does not need the general runtime's per-left output-key set or
/// temporary desired-row map.
const PARALLEL_JOIN_WORK_ENTER: usize = 65_536;
const PARALLEL_JOIN_WORK_EXIT: usize = 49_152;

fn commit_keyed_value<LK, LV, RK, RV, JK, OV>(
    state: &mut JoinState<LK, LV, RK, RV, JK, LK, OV>,
    left_key: LK,
    desired_value: Option<OV>,
    changes: &mut Vec<MapDiff<LK, OV>>,
) where
    LK: Hash + Eq + CellValue,
    LV: CellValue,
    RK: Hash + Eq + CellValue,
    RV: CellValue,
    JK: Hash + Eq + CellValue,
    OV: CellValue,
{
    match (state.output_cache.entry(left_key.clone()), desired_value) {
        (Entry::Occupied(mut entry), Some(new_value)) => {
            if entry.get() != &new_value {
                let old_value = entry.insert(new_value.clone());
                changes.push(MapDiff::Update {
                    key: left_key,
                    old_value,
                    new_value,
                });
            }
        }
        (Entry::Occupied(entry), None) => {
            let (key, old_value) = entry.remove_entry();
            changes.push(MapDiff::Remove { key, old_value });
        }
        (Entry::Vacant(entry), Some(new_value)) => {
            entry.insert(new_value.clone());
            changes.push(MapDiff::Insert {
                key: left_key,
                value: new_value,
            });
        }
        (Entry::Vacant(_), None) => {}
    }
}

#[cfg(all(feature = "scheduler", not(target_arch = "wasm32")))]
fn compute_keyed_parallel<LK, LV, RK, RV, JK, OV, FO>(
    pool: &rayon::ThreadPool,
    state: &JoinState<LK, LV, RK, RV, JK, LK, OV>,
    impacted: &[LK],
    compute_value: &FO,
) -> Vec<(LK, Option<OV>)>
where
    LK: Hash + Eq + CellValue,
    LV: CellValue,
    RK: Hash + Eq + CellValue,
    RV: CellValue,
    JK: Hash + Eq + CellValue,
    OV: CellValue,
    FO: Fn(&LK, &LV, &[(RK, RV)]) -> Option<OV> + Sync,
{
    use rayon::prelude::*;

    pool.install(|| {
        impacted
            .par_iter()
            .map_init(Vec::<(RK, RV)>::new, |right_rows, left_key| {
                right_rows.clear();
                let desired = state.left_rows.get(left_key).and_then(|left_value| {
                    if let Some(right_keys) = state
                        .left_join_keys
                        .get(left_key)
                        .and_then(|join_key| state.join_to_right.get(join_key))
                    {
                        right_rows.extend(right_keys.iter().filter_map(|right_key| {
                            state
                                .right_rows
                                .get(right_key)
                                .map(|right_value| (right_key.clone(), right_value.clone()))
                        }));
                    }
                    compute_value(left_key, left_value, right_rows)
                });
                (left_key.clone(), desired)
            })
            .collect()
    })
}

fn recompute_keyed_impacted<LK, LV, RK, RV, JK, OV, FO>(
    state: &mut JoinState<LK, LV, RK, RV, JK, LK, OV>,
    scratch: &mut JoinScratch<LK, RK, RV, JK, LK, OV>,
    compute_value: &FO,
) -> Vec<MapDiff<LK, OV>>
where
    LK: Hash + Eq + CellValue,
    LV: CellValue,
    RK: Hash + Eq + CellValue,
    RV: CellValue,
    JK: Hash + Eq + CellValue,
    OV: CellValue,
    FO: Fn(&LK, &LV, &[(RK, RV)]) -> Option<OV> + Sync,
{
    let mut changes = Vec::new();
    let impacted: Vec<LK> = scratch.impacted.drain().collect();
    let estimated_work = impacted.iter().fold(0_usize, |work, left_key| {
        let fanout = state
            .left_join_keys
            .get(left_key)
            .and_then(|join_key| state.join_to_right.get(join_key))
            .map_or(0, Vec::len);
        work.saturating_add(fanout.saturating_add(1))
    });
    state.parallel_active = if state.parallel_active {
        estimated_work >= PARALLEL_JOIN_WORK_EXIT
    } else {
        estimated_work >= PARALLEL_JOIN_WORK_ENTER
    };

    #[cfg(all(feature = "scheduler", not(target_arch = "wasm32")))]
    if state.parallel_active
        && let Some(pool) = crate::executor::worker_pool()
    {
        let desired = compute_keyed_parallel(pool, state, &impacted, compute_value);
        for (left_key, desired_value) in desired {
            commit_keyed_value(state, left_key, desired_value, &mut changes);
        }
        return changes;
    }

    for left_key in impacted {
        scratch.right_rows.clear();
        let desired_value = state.left_rows.get(&left_key).and_then(|left_value| {
            if let Some(right_keys) = state
                .left_join_keys
                .get(&left_key)
                .and_then(|join_key| state.join_to_right.get(join_key))
            {
                scratch
                    .right_rows
                    .extend(right_keys.iter().filter_map(|right_key| {
                        state
                            .right_rows
                            .get(right_key)
                            .map(|right_value| (right_key.clone(), right_value.clone()))
                    }));
            }
            compute_value(&left_key, left_value, &scratch.right_rows)
        });

        commit_keyed_value(state, left_key, desired_value, &mut changes);
    }

    changes
}

// ── The public entry point ──────────────────────────────────────────────

/// Emit a batch of output diffs through `sink`.
///
/// Preserves the original `apply_batch` semantics observed by downstream
/// subscribers: every non-empty group of output diffs produced from a single
/// upstream diff is delivered as one `MapDiff::Batch`, even when the group
/// contains a single change. Empty batches are dropped.
fn emit_changes<OK, OV, Sink>(sink: &Sink, changes: Vec<MapDiff<OK, OV>>)
where
    OK: Hash + Eq + CellValue,
    OV: CellValue,
    Sink: crate::map_query::MapDiffSink<OK, OV>,
{
    if changes.is_empty() {
        return;
    }
    sink(&MapDiff::Batch { changes });
}

/// Install join machinery that drives `sink` instead of allocating an output map.
///
/// Subscribes to both source maps via
/// [`MapQuery::install`](crate::map_query::MapQuery::install), maintains the
/// join state, and pushes resulting `MapDiff`s into the sink. Returns the
/// subscription guards (caller owns them — typically attaches them to the
/// materialized output). Chains of plans compose without intermediate
/// [`CellMap`](crate::CellMap) allocations.
///
/// Used by `MapQuery` join plan nodes whose materialization owns a single
/// output cell map; multiple plan stages share that output rather than each
/// allocating their own.
pub fn install_join_runtime_via_query<LK, LV, RK, RV, JK, OK, OV, L, R, FL, FR, FO, Sink>(
    left: L,
    right: R,
    left_join_key: FL,
    right_join_key: FR,
    compute_rows: FO,
    sink: Sink,
) -> Vec<SubscriptionGuard>
where
    LK: Hash + Eq + CellValue,
    LV: CellValue,
    RK: Hash + Eq + CellValue,
    RV: CellValue,
    JK: Hash + Eq + CellValue,
    OK: Hash + Eq + CellValue,
    OV: CellValue,
    L: crate::map_query::MapQuery<Key = LK, Value = LV>,
    R: crate::map_query::MapQuery<Key = RK, Value = RV>,
    FL: Fn(&LK, &LV) -> JK + Send + Sync + 'static,
    FR: Fn(&RK, &RV) -> JK + Send + Sync + 'static,
    FO: Fn(&LK, &LV, &[(RK, RV)]) -> Vec<(OK, OV)> + Send + Sync + 'static,
    Sink: crate::map_query::MapDiffSink<OK, OV>,
{
    let state = Arc::new(Mutex::new(
        JoinState::<LK, LV, RK, RV, JK, OK, OV>::default(),
    ));
    let left_join_key = Arc::new(left_join_key);
    let right_join_key = Arc::new(right_join_key);
    let compute_rows = Arc::new(compute_rows);
    let sink = Arc::new(sink);

    let left_sink = {
        let state = state.clone();
        let left_join_key = left_join_key;
        let compute_rows = compute_rows.clone();
        let sink = sink.clone();
        move |diff: &MapDiff<LK, LV>| {
            let mut state = state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut scratch = std::mem::take(&mut state.scratch);
            apply_left_diff(
                &mut state,
                diff,
                left_join_key.as_ref(),
                &mut scratch.impacted,
            );
            let changes = recompute_impacted(&mut state, &mut scratch, compute_rows.as_ref());
            state.scratch = scratch;
            // Emit while STILL holding `state`, not after dropping it. Under the
            // scheduler's wave-parallel drain the left and right sinks can run on
            // two threads at once; each reads the sibling's rows under this lock
            // to build `changes`, so the emit must land under the same lock too.
            // Otherwise two concurrent sibling emits touching one output key can
            // reorder vs their lock order and last-write-wins a stale combined
            // row into the output map — the CellMap analogue of the join.rs
            // torn-value bug. Holding it across the emit makes emit order == lock
            // order, so whichever side observed the freshest sibling emits last.
            emit_changes(sink.as_ref(), changes);
            drop(state);
        }
    };

    let right_sink = {
        let state = state;
        let right_join_key = right_join_key;
        let compute_rows = compute_rows;
        let sink = sink;
        move |diff: &MapDiff<RK, RV>| {
            let mut state = state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut scratch = std::mem::take(&mut state.scratch);
            apply_right_diff(
                &mut state,
                diff,
                right_join_key.as_ref(),
                &mut scratch.impacted,
                &mut scratch.changed_join_keys,
            );
            let changes = recompute_impacted(&mut state, &mut scratch, compute_rows.as_ref());
            state.scratch = scratch;
            // Emit while STILL holding `state`, not after dropping it. Under the
            // scheduler's wave-parallel drain the left and right sinks can run on
            // two threads at once; each reads the sibling's rows under this lock
            // to build `changes`, so the emit must land under the same lock too.
            // Otherwise two concurrent sibling emits touching one output key can
            // reorder vs their lock order and last-write-wins a stale combined
            // row into the output map — the CellMap analogue of the join.rs
            // torn-value bug. Holding it across the emit makes emit order == lock
            // order, so whichever side observed the freshest sibling emits last.
            emit_changes(sink.as_ref(), changes);
            drop(state);
        }
    };

    let mut guards = left.install(left_sink);
    guards.extend(right.install(right_sink));
    guards
}

/// Install the zero-or-one-output, left-key-preserving join runtime.
pub fn install_keyed_join_runtime_via_query<LK, LV, RK, RV, JK, OV, L, R, FL, FR, FO, Sink>(
    left: L,
    right: R,
    left_join_key: FL,
    right_join_key: FR,
    compute_value: FO,
    sink: Sink,
) -> Vec<SubscriptionGuard>
where
    LK: Hash + Eq + CellValue,
    LV: CellValue,
    RK: Hash + Eq + CellValue,
    RV: CellValue,
    JK: Hash + Eq + CellValue,
    OV: CellValue,
    L: crate::map_query::MapQuery<Key = LK, Value = LV>,
    R: crate::map_query::MapQuery<Key = RK, Value = RV>,
    FL: Fn(&LK, &LV) -> JK + Send + Sync + 'static,
    FR: Fn(&RK, &RV) -> JK + Send + Sync + 'static,
    FO: Fn(&LK, &LV, &[(RK, RV)]) -> Option<OV> + Send + Sync + 'static,
    Sink: crate::map_query::MapDiffSink<LK, OV>,
{
    let state = Arc::new(Mutex::new(
        JoinState::<LK, LV, RK, RV, JK, LK, OV>::default(),
    ));
    let left_join_key = Arc::new(left_join_key);
    let right_join_key = Arc::new(right_join_key);
    let compute_value = Arc::new(compute_value);
    let sink = Arc::new(sink);

    let left_sink = {
        let state = state.clone();
        let left_join_key = left_join_key;
        let compute_value = compute_value.clone();
        let sink = sink.clone();
        move |diff: &MapDiff<LK, LV>| {
            let mut state = state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut scratch = std::mem::take(&mut state.scratch);
            apply_left_diff(
                &mut state,
                diff,
                left_join_key.as_ref(),
                &mut scratch.impacted,
            );
            let changes =
                recompute_keyed_impacted(&mut state, &mut scratch, compute_value.as_ref());
            state.scratch = scratch;
            emit_changes(sink.as_ref(), changes);
            drop(state);
        }
    };

    let right_sink = {
        let state = state;
        let right_join_key = right_join_key;
        let compute_value = compute_value;
        let sink = sink;
        move |diff: &MapDiff<RK, RV>| {
            let mut state = state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut scratch = std::mem::take(&mut state.scratch);
            apply_right_diff(
                &mut state,
                diff,
                right_join_key.as_ref(),
                &mut scratch.impacted,
                &mut scratch.changed_join_keys,
            );
            let changes =
                recompute_keyed_impacted(&mut state, &mut scratch, compute_value.as_ref());
            state.scratch = scratch;
            emit_changes(sink.as_ref(), changes);
            drop(state);
        }
    };

    let mut guards = left.install(left_sink);
    guards.extend(right.install(right_sink));
    guards
}

/// Install two consecutive left-key-preserving joins as one coordinated
/// runtime. All three roots enter one state lock directly; intermediate rows
/// are carried from the first join into the second without a subscriber or
/// dynamically dispatched callback boundary.
#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    clippy::type_complexity
)]
pub fn install_two_keyed_join_runtime_via_query<
    LK,
    LV,
    RK1,
    RV1,
    JK1,
    MV,
    RK2,
    RV2,
    JK2,
    OV,
    L,
    R1,
    R2,
    FL1,
    FR1,
    FM1,
    FL2,
    FR2,
    FM2,
    Sink,
>(
    left: L,
    right1: R1,
    right2: R2,
    left_join_key1: FL1,
    right_join_key1: FR1,
    map_first: FM1,
    left_join_key2: FL2,
    right_join_key2: FR2,
    map_second: FM2,
    sink: Sink,
) -> Vec<SubscriptionGuard>
where
    LK: Hash + Eq + CellValue,
    LV: CellValue,
    RK1: Hash + Eq + CellValue,
    RV1: CellValue,
    JK1: Hash + Eq + CellValue,
    MV: CellValue,
    RK2: Hash + Eq + CellValue,
    RV2: CellValue,
    JK2: Hash + Eq + CellValue,
    OV: CellValue,
    L: crate::map_query::MapQuery<Key = LK, Value = LV>,
    R1: crate::map_query::MapQuery<Key = RK1, Value = RV1>,
    R2: crate::map_query::MapQuery<Key = RK2, Value = RV2>,
    FL1: Fn(&LK, &LV) -> JK1 + Send + Sync + 'static,
    FR1: Fn(&RK1, &RV1) -> JK1 + Send + Sync + 'static,
    FM1: Fn(&LK, &LV, &[(RK1, RV1)]) -> MV + Send + Sync + 'static,
    FL2: Fn(&LK, &MV) -> JK2 + Send + Sync + 'static,
    FR2: Fn(&RK2, &RV2) -> JK2 + Send + Sync + 'static,
    FM2: Fn(&LK, &MV, &[(RK2, RV2)]) -> OV + Send + Sync + 'static,
    Sink: crate::map_query::MapDiffSink<LK, OV>,
{
    type FirstState<LK, LV, RK, RV, JK, MV> = JoinState<LK, LV, RK, RV, JK, LK, MV>;
    type SecondState<LK, MV, RK, RV, JK, OV> = JoinState<LK, MV, RK, RV, JK, LK, OV>;

    let state = Arc::new(Mutex::new((
        FirstState::<LK, LV, RK1, RV1, JK1, MV>::default(),
        SecondState::<LK, MV, RK2, RV2, JK2, OV>::default(),
    )));
    let left_join_key1 = Arc::new(left_join_key1);
    let right_join_key1 = Arc::new(right_join_key1);
    let map_first = Arc::new(map_first);
    let left_join_key2 = Arc::new(left_join_key2);
    let right_join_key2 = Arc::new(right_join_key2);
    let map_second = Arc::new(map_second);
    let sink = Arc::new(sink);

    let propagate_first = {
        let map_first = Arc::clone(&map_first);
        let left_join_key2 = Arc::clone(&left_join_key2);
        let map_second = Arc::clone(&map_second);
        move |first: &mut FirstState<LK, LV, RK1, RV1, JK1, MV>,
              second: &mut SecondState<LK, MV, RK2, RV2, JK2, OV>| {
            let mut scratch1 = std::mem::take(&mut first.scratch);
            let intermediate =
                recompute_keyed_impacted(first, &mut scratch1, &|key, value, rights| {
                    Some(map_first(key, value, rights))
                });
            first.scratch = scratch1;

            let mut scratch2 = std::mem::take(&mut second.scratch);
            for change in &intermediate {
                apply_left_diff(
                    second,
                    change,
                    left_join_key2.as_ref(),
                    &mut scratch2.impacted,
                );
            }
            let output = recompute_keyed_impacted(second, &mut scratch2, &|key, value, rights| {
                Some(map_second(key, value, rights))
            });
            second.scratch = scratch2;
            output
        }
    };
    let propagate_first = Arc::new(propagate_first);

    let left_sink = {
        let state = Arc::clone(&state);
        let left_join_key1 = Arc::clone(&left_join_key1);
        let propagate_first = Arc::clone(&propagate_first);
        let sink = Arc::clone(&sink);
        move |diff: &MapDiff<LK, LV>| {
            let changes = {
                let mut state = state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let (first, second) = &mut *state;
                let mut scratch = std::mem::take(&mut first.scratch);
                apply_left_diff(first, diff, left_join_key1.as_ref(), &mut scratch.impacted);
                first.scratch = scratch;
                let changes = propagate_first(first, second);
                drop(state);
                changes
            };
            emit_changes(sink.as_ref(), changes);
        }
    };

    let right1_sink = {
        let state = Arc::clone(&state);
        let right_join_key1 = Arc::clone(&right_join_key1);
        let propagate_first = Arc::clone(&propagate_first);
        let sink = Arc::clone(&sink);
        move |diff: &MapDiff<RK1, RV1>| {
            let changes = {
                let mut state = state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let (first, second) = &mut *state;
                let mut scratch = std::mem::take(&mut first.scratch);
                apply_right_diff(
                    first,
                    diff,
                    right_join_key1.as_ref(),
                    &mut scratch.impacted,
                    &mut scratch.changed_join_keys,
                );
                first.scratch = scratch;
                let changes = propagate_first(first, second);
                drop(state);
                changes
            };
            emit_changes(sink.as_ref(), changes);
        }
    };

    let right2_sink = {
        let state = state;
        let right_join_key2 = right_join_key2;
        let map_second = map_second;
        let sink = sink;
        move |diff: &MapDiff<RK2, RV2>| {
            let changes = {
                let mut state = state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let second = &mut state.1;
                let mut scratch = std::mem::take(&mut second.scratch);
                apply_right_diff(
                    second,
                    diff,
                    right_join_key2.as_ref(),
                    &mut scratch.impacted,
                    &mut scratch.changed_join_keys,
                );
                let changes =
                    recompute_keyed_impacted(second, &mut scratch, &|key, value, rights| {
                        Some(map_second(key, value, rights))
                    });
                second.scratch = scratch;
                drop(state);
                changes
            };
            emit_changes(sink.as_ref(), changes);
        }
    };

    let mut guards = left.install(left_sink);
    guards.extend(right1.install(right1_sink));
    guards.extend(right2.install(right2_sink));
    guards
}
