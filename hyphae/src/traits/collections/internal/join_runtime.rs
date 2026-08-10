use std::{
    any::TypeId,
    collections::hash_map::Entry,
    hash::{Hash, Hasher},
    sync::{Arc, Mutex},
};

use rustc_hash::{FxHashMap, FxHasher};

use crate::{cell_map::MapDiff, subscription::SubscriptionGuard, traits::CellValue};

use super::ordered_set::OrderedSet;

struct JoinState<LK, LV, RK, RV, JK, OK, OV, RI = RelationIndex<RK, RV, JK>>
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
    right: RI,
    left_output_keys: FxHashMap<LK, OrderedSet<OK>>,
    output_cache: FxHashMap<OK, OV>,
    parallel_active: bool,
    scratch: JoinScratch<LK, RK, RV, JK, OK, OV>,
}

/// Typed physical index for one right-side relationship.
struct RelationIndex<RK, RV, JK> {
    rows: FxHashMap<RK, RV>,
    row_join_keys: FxHashMap<RK, JK>,
    join_to_rows: FxHashMap<JK, Vec<RK>>,
}

impl<RK, RV, JK> Default for RelationIndex<RK, RV, JK> {
    fn default() -> Self {
        Self {
            rows: FxHashMap::default(),
            row_join_keys: FxHashMap::default(),
            join_to_rows: FxHashMap::default(),
        }
    }
}

trait RelationIndexStorage<RK, RV, JK>: Send + Sync + 'static {
    fn read<T>(&self, read: impl FnOnce(&RelationIndex<RK, RV, JK>) -> T) -> T;
    fn write<T>(&mut self, write: impl FnOnce(&mut RelationIndex<RK, RV, JK>) -> T) -> T;
}

impl<RK, RV, JK> RelationIndexStorage<RK, RV, JK> for RelationIndex<RK, RV, JK>
where
    RK: Send + Sync + 'static,
    RV: Send + Sync + 'static,
    JK: Send + Sync + 'static,
{
    fn read<T>(&self, read: impl FnOnce(&Self) -> T) -> T {
        read(self)
    }

    fn write<T>(&mut self, write: impl FnOnce(&mut Self) -> T) -> T {
        write(self)
    }
}

impl<RK, RV, JK> RelationIndexStorage<RK, RV, JK>
    for crate::map_query::compiler::DeferredPhysical<RelationIndex<RK, RV, JK>>
where
    RK: Send + Sync + 'static,
    RV: Send + Sync + 'static,
    JK: Send + Sync + 'static,
{
    fn read<T>(&self, read: impl FnOnce(&RelationIndex<RK, RV, JK>) -> T) -> T {
        crate::map_query::compiler::DeferredPhysical::read(self, read)
    }

    fn write<T>(&mut self, write: impl FnOnce(&mut RelationIndex<RK, RV, JK>) -> T) -> T {
        crate::map_query::compiler::DeferredPhysical::write(self, write)
    }
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
            right: RelationIndex::default(),
            left_output_keys: FxHashMap::default(),
            output_cache: FxHashMap::default(),
            parallel_active: false,
            scratch: JoinScratch::default(),
        }
    }
}

impl<LK, LV, RK, RV, JK, OK, OV, RI> JoinState<LK, LV, RK, RV, JK, OK, OV, RI>
where
    LK: Hash + Eq + CellValue,
    LV: CellValue,
    RK: Hash + Eq + CellValue,
    RV: CellValue,
    JK: Hash + Eq + CellValue,
    OK: Hash + Eq + CellValue,
    OV: CellValue,
{
    fn with_right(right: RI) -> Self {
        Self {
            left_rows: FxHashMap::default(),
            left_join_keys: FxHashMap::default(),
            join_to_left: FxHashMap::default(),
            right,
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
    state: &mut JoinState<LK, LV, RK, RV, JK, OK, OV, impl RelationIndexStorage<RK, RV, JK>>,
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
    state: &mut JoinState<LK, LV, RK, RV, JK, OK, OV, impl RelationIndexStorage<RK, RV, JK>>,
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
    state: &mut JoinState<LK, LV, RK, RV, JK, OK, OV, impl RelationIndexStorage<RK, RV, JK>>,
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
    state: &mut JoinState<LK, LV, RK, RV, JK, OK, OV, impl RelationIndexStorage<RK, RV, JK>>,
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
    state.right.write(|right| {
        match right
            .row_join_keys
            .insert(right_key.clone(), join_key.clone())
        {
            Some(previous_join_key) if previous_join_key != join_key => {
                remove_index_member(&mut right.join_to_rows, &previous_join_key, &right_key);
                changed_join_keys.insert(previous_join_key);
                add_index_member(&mut right.join_to_rows, join_key.clone(), right_key.clone());
            }
            Some(_) => {}
            None => add_index_member(&mut right.join_to_rows, join_key.clone(), right_key.clone()),
        }
        right.rows.insert(right_key, right_value);
    });
    changed_join_keys.insert(join_key);
}

fn remove_right<LK, LV, RK, RV, JK, OK, OV>(
    state: &mut JoinState<LK, LV, RK, RV, JK, OK, OV, impl RelationIndexStorage<RK, RV, JK>>,
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
    state.right.write(|right| {
        if let Some(previous_join_key) = right.row_join_keys.remove(right_key) {
            remove_index_member(&mut right.join_to_rows, &previous_join_key, right_key);
            changed_join_keys.insert(previous_join_key);
        }
        right.rows.remove(right_key);
    });
}

fn apply_right_diff<LK, LV, RK, RV, JK, OK, OV, FR>(
    state: &mut JoinState<LK, LV, RK, RV, JK, OK, OV, impl RelationIndexStorage<RK, RV, JK>>,
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
        state: &mut JoinState<LK, LV, RK, RV, JK, OK, OV, impl RelationIndexStorage<RK, RV, JK>>,
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
                state.right.write(|right| {
                    for join_key in right.row_join_keys.values() {
                        changed_join_keys.insert(join_key.clone());
                    }
                    right.rows.clear();
                    right.row_join_keys.clear();
                    right.join_to_rows.clear();
                });
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
    state: &mut JoinState<LK, LV, RK, RV, JK, OK, OV, impl RelationIndexStorage<RK, RV, JK>>,
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
            if let Some(join_key) = state.left_join_keys.get(&left_key) {
                state.right.read(|right| {
                    if let Some(right_keys) = right.join_to_rows.get(join_key) {
                        scratch
                            .right_rows
                            .extend(right_keys.iter().filter_map(|right_key| {
                                right
                                    .rows
                                    .get(right_key)
                                    .map(|right_value| (right_key.clone(), right_value.clone()))
                            }));
                    }
                });
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
    state: &mut JoinState<LK, LV, RK, RV, JK, LK, OV, impl RelationIndexStorage<RK, RV, JK>>,
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
    state: &JoinState<LK, LV, RK, RV, JK, LK, OV, impl RelationIndexStorage<RK, RV, JK>>,
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
                    if let Some(join_key) = state.left_join_keys.get(left_key) {
                        state.right.read(|right| {
                            if let Some(right_keys) = right.join_to_rows.get(join_key) {
                                right_rows.extend(right_keys.iter().filter_map(|right_key| {
                                    right
                                        .rows
                                        .get(right_key)
                                        .map(|right_value| (right_key.clone(), right_value.clone()))
                                }));
                            }
                        });
                    }
                    compute_value(left_key, left_value, right_rows)
                });
                (left_key.clone(), desired)
            })
            .collect()
    })
}

fn recompute_keyed_impacted<LK, LV, RK, RV, JK, OV, FO>(
    state: &mut JoinState<LK, LV, RK, RV, JK, LK, OV, impl RelationIndexStorage<RK, RV, JK>>,
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
    let estimated_work = state.right.read(|right| {
        impacted.iter().fold(0_usize, |work, left_key| {
            let fanout = state
                .left_join_keys
                .get(left_key)
                .and_then(|join_key| right.join_to_rows.get(join_key))
                .map_or(0, Vec::len);
            work.saturating_add(fanout.saturating_add(1))
        })
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
            if let Some(join_key) = state.left_join_keys.get(&left_key) {
                state.right.read(|right| {
                    if let Some(right_keys) = right.join_to_rows.get(join_key) {
                        scratch
                            .right_rows
                            .extend(right_keys.iter().filter_map(|right_key| {
                                right
                                    .rows
                                    .get(right_key)
                                    .map(|right_value| (right_key.clone(), right_value.clone()))
                            }));
                    }
                });
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
/// Compiles both source maps into direct entry points, maintains the
/// join state, and pushes resulting `MapDiff`s into the sink. Returns the
/// subscription guards (caller owns them — typically attaches them to the
/// materialized output). Chains of plans compose without intermediate
/// [`CellMap`](crate::CellMap) allocations.
///
/// Used by `MapQuery` join plan nodes whose materialization owns a single
/// output cell map; multiple plan stages share that output rather than each
/// allocating their own.
pub fn install_join_runtime_via_query<LK, LV, RK, RV, JK, OK, OV, L, R, FL, FR, FO, Sink>(
    cx: &mut crate::map_query::compiler::CompileContext,
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

    let mut guards = left.compile_into(cx, left_sink);
    guards.extend(right.compile_into(cx, right_sink));
    guards
}

fn query_shard_count() -> usize {
    #[cfg(all(feature = "scheduler", not(target_arch = "wasm32")))]
    if let Some(pool) = crate::executor::worker_pool() {
        return pool.current_num_threads().max(1);
    }
    1
}

fn shard_for<T: Hash>(value: &T, shard_count: usize) -> usize {
    let mut hasher = FxHasher::default();
    value.hash(&mut hasher);
    let count = u64::try_from(shard_count.max(1)).unwrap_or(1);
    let index = hasher.finish().checked_rem(count).unwrap_or(0);
    usize::try_from(index).unwrap_or(0)
}

const fn diff_key<K, V>(diff: &MapDiff<K, V>) -> Option<&K> {
    match diff {
        MapDiff::Insert { key, .. } | MapDiff::Update { key, .. } | MapDiff::Remove { key, .. } => {
            Some(key)
        }
        MapDiff::Initial { .. } | MapDiff::Batch { .. } => None,
    }
}

const fn diff_merge_phase<K, V>(diff: &MapDiff<K, V>) -> u8 {
    match diff {
        MapDiff::Initial { .. } => 0,
        MapDiff::Remove { .. } => 1,
        MapDiff::Update { .. } => 2,
        MapDiff::Insert { .. } => 3,
        MapDiff::Batch { .. } => 4,
    }
}

fn push_routed<T>(routed: &mut [Vec<T>], route: usize, value: T) {
    debug_assert!(route < routed.len(), "join shard route must be in bounds");
    if let Some(shard) = routed.get_mut(route) {
        shard.push(value);
    }
}

struct ShardedKeyedJoin<LK, LV, RK, RV, JK, OV>
where
    LK: Hash + Eq + CellValue,
    LV: CellValue,
    RK: Hash + Eq + CellValue,
    RV: CellValue,
    JK: Hash + Eq + CellValue,
    OV: CellValue,
{
    shards: Vec<JoinState<LK, LV, RK, RV, JK, LK, OV>>,
    left_routes: FxHashMap<LK, usize>,
    right_routes: FxHashMap<RK, usize>,
    left_sequence: FxHashMap<LK, u64>,
    next_sequence: u64,
}

impl<LK, LV, RK, RV, JK, OV> ShardedKeyedJoin<LK, LV, RK, RV, JK, OV>
where
    LK: Hash + Eq + CellValue,
    LV: CellValue,
    RK: Hash + Eq + CellValue,
    RV: CellValue,
    JK: Hash + Eq + CellValue,
    OV: CellValue,
{
    fn new(shard_count: usize) -> Self {
        Self {
            shards: (0..shard_count.max(1))
                .map(|_| JoinState::default())
                .collect(),
            left_routes: FxHashMap::default(),
            right_routes: FxHashMap::default(),
            left_sequence: FxHashMap::default(),
            next_sequence: 0,
        }
    }

    fn route_left<FL>(
        &mut self,
        diff: &MapDiff<LK, LV>,
        left_join_key: &FL,
    ) -> (Vec<Vec<MapDiff<LK, LV>>>, FxHashMap<LK, u64>)
    where
        FL: Fn(&LK, &LV) -> JK,
    {
        let mut flattened = Vec::new();
        crate::traits::collections::internal::map_runtime::flatten_diff(diff, &mut flattened);
        self.route_left_owned(flattened, left_join_key)
    }

    #[allow(clippy::too_many_lines)]
    fn route_left_owned<FL>(
        &mut self,
        flattened: Vec<MapDiff<LK, LV>>,
        left_join_key: &FL,
    ) -> (Vec<Vec<MapDiff<LK, LV>>>, FxHashMap<LK, u64>)
    where
        FL: Fn(&LK, &LV) -> JK,
    {
        let mut routed = (0..self.shards.len())
            .map(|_| Vec::new())
            .collect::<Vec<_>>();
        let mut event_ordinals = FxHashMap::default();

        for (event_index, change) in flattened.into_iter().enumerate() {
            let event_ordinal = u64::try_from(event_index).unwrap_or(u64::MAX);
            match change {
                MapDiff::Initial { entries } => {
                    for shard in &mut routed {
                        shard.push(MapDiff::Initial {
                            entries: Vec::new(),
                        });
                    }
                    self.left_routes.clear();
                    self.left_sequence.clear();
                    self.next_sequence = 0;
                    for (key, value) in entries {
                        let join_key = left_join_key(&key, &value);
                        let route = shard_for(&join_key, self.shards.len());
                        push_routed(
                            &mut routed,
                            route,
                            MapDiff::Insert {
                                key: key.clone(),
                                value,
                            },
                        );
                        self.left_routes.insert(key.clone(), route);
                        self.left_sequence.insert(key.clone(), self.next_sequence);
                        event_ordinals.insert(key, self.next_sequence);
                        self.next_sequence = self.next_sequence.wrapping_add(1);
                    }
                }
                MapDiff::Insert { key, value } => {
                    let join_key = left_join_key(&key, &value);
                    let route = shard_for(&join_key, self.shards.len());
                    push_routed(
                        &mut routed,
                        route,
                        MapDiff::Insert {
                            key: key.clone(),
                            value,
                        },
                    );
                    self.left_routes.insert(key.clone(), route);
                    if !self.left_sequence.contains_key(&key) {
                        self.left_sequence.insert(key.clone(), self.next_sequence);
                        self.next_sequence = self.next_sequence.wrapping_add(1);
                    }
                    event_ordinals.entry(key).or_insert(event_ordinal);
                }
                MapDiff::Update {
                    key,
                    old_value,
                    new_value,
                } => {
                    let new_join_key = left_join_key(&key, &new_value);
                    let new_route = shard_for(&new_join_key, self.shards.len());
                    let old_route = self.left_routes.get(&key).copied().unwrap_or_else(|| {
                        shard_for(&left_join_key(&key, &old_value), self.shards.len())
                    });
                    if old_route == new_route {
                        push_routed(
                            &mut routed,
                            new_route,
                            MapDiff::Update {
                                key: key.clone(),
                                old_value,
                                new_value,
                            },
                        );
                    } else {
                        push_routed(
                            &mut routed,
                            old_route,
                            MapDiff::Remove {
                                key: key.clone(),
                                old_value,
                            },
                        );
                        push_routed(
                            &mut routed,
                            new_route,
                            MapDiff::Insert {
                                key: key.clone(),
                                value: new_value,
                            },
                        );
                    }
                    self.left_routes.insert(key.clone(), new_route);
                    event_ordinals.entry(key).or_insert(event_ordinal);
                }
                MapDiff::Remove { key, old_value } => {
                    let route = self.left_routes.remove(&key).unwrap_or_else(|| {
                        shard_for(&left_join_key(&key, &old_value), self.shards.len())
                    });
                    push_routed(
                        &mut routed,
                        route,
                        MapDiff::Remove {
                            key: key.clone(),
                            old_value,
                        },
                    );
                    self.left_sequence.remove(&key);
                    event_ordinals.entry(key).or_insert(event_ordinal);
                }
                MapDiff::Batch { .. } => {}
            }
        }
        (routed, event_ordinals)
    }

    fn route_right<FR>(
        &mut self,
        diff: &MapDiff<RK, RV>,
        right_join_key: &FR,
    ) -> Vec<Vec<MapDiff<RK, RV>>>
    where
        FR: Fn(&RK, &RV) -> JK,
    {
        let mut routed = (0..self.shards.len())
            .map(|_| Vec::new())
            .collect::<Vec<_>>();
        let mut flattened = Vec::new();
        crate::traits::collections::internal::map_runtime::flatten_diff(diff, &mut flattened);

        for change in flattened {
            match change {
                MapDiff::Initial { entries } => {
                    for shard in &mut routed {
                        shard.push(MapDiff::Initial {
                            entries: Vec::new(),
                        });
                    }
                    self.right_routes.clear();
                    for (key, value) in entries {
                        let route = shard_for(&right_join_key(&key, &value), self.shards.len());
                        push_routed(
                            &mut routed,
                            route,
                            MapDiff::Insert {
                                key: key.clone(),
                                value,
                            },
                        );
                        self.right_routes.insert(key, route);
                    }
                }
                MapDiff::Insert { key, value } => {
                    let route = shard_for(&right_join_key(&key, &value), self.shards.len());
                    push_routed(
                        &mut routed,
                        route,
                        MapDiff::Insert {
                            key: key.clone(),
                            value,
                        },
                    );
                    self.right_routes.insert(key, route);
                }
                MapDiff::Update {
                    key,
                    old_value,
                    new_value,
                } => {
                    let new_route = shard_for(&right_join_key(&key, &new_value), self.shards.len());
                    let old_route = self.right_routes.get(&key).copied().unwrap_or_else(|| {
                        shard_for(&right_join_key(&key, &old_value), self.shards.len())
                    });
                    if old_route == new_route {
                        push_routed(
                            &mut routed,
                            new_route,
                            MapDiff::Update {
                                key: key.clone(),
                                old_value,
                                new_value,
                            },
                        );
                    } else {
                        push_routed(
                            &mut routed,
                            old_route,
                            MapDiff::Remove {
                                key: key.clone(),
                                old_value,
                            },
                        );
                        push_routed(
                            &mut routed,
                            new_route,
                            MapDiff::Insert {
                                key: key.clone(),
                                value: new_value,
                            },
                        );
                    }
                    self.right_routes.insert(key, new_route);
                }
                MapDiff::Remove { key, old_value } => {
                    let route = self.right_routes.remove(&key).unwrap_or_else(|| {
                        shard_for(&right_join_key(&key, &old_value), self.shards.len())
                    });
                    push_routed(&mut routed, route, MapDiff::Remove { key, old_value });
                }
                MapDiff::Batch { .. } => {}
            }
        }
        routed
    }

    fn merge_changes(
        &self,
        shard_changes: Vec<Vec<MapDiff<LK, OV>>>,
        event_ordinals: &FxHashMap<LK, u64>,
    ) -> Vec<MapDiff<LK, OV>> {
        let mut tagged = Vec::new();
        for changes in shard_changes {
            for (local_ordinal, change) in changes.into_iter().enumerate() {
                let ordinal = diff_key(&change)
                    .and_then(|key| {
                        event_ordinals
                            .get(key)
                            .or_else(|| self.left_sequence.get(key))
                    })
                    .copied()
                    .unwrap_or(u64::MAX);
                let phase = diff_merge_phase(&change);
                tagged.push((ordinal, phase, local_ordinal, change));
            }
        }
        tagged.sort_by_key(|(ordinal, phase, local, _)| (*ordinal, *phase, *local));
        tagged.into_iter().map(|(_, _, _, change)| change).collect()
    }
}

fn process_left_shards<LK, LV, RK, RV, JK, OV, FL, FO>(
    join: &mut ShardedKeyedJoin<LK, LV, RK, RV, JK, OV>,
    routed: &[Vec<MapDiff<LK, LV>>],
    left_join_key: &FL,
    compute_value: &FO,
) -> Vec<Vec<MapDiff<LK, OV>>>
where
    LK: Hash + Eq + CellValue,
    LV: CellValue,
    RK: Hash + Eq + CellValue,
    RV: CellValue,
    JK: Hash + Eq + CellValue,
    OV: CellValue,
    FL: Fn(&LK, &LV) -> JK + Sync,
    FO: Fn(&LK, &LV, &[(RK, RV)]) -> OV + Sync,
{
    let process = |(state, diffs): (
        &mut JoinState<LK, LV, RK, RV, JK, LK, OV>,
        &Vec<MapDiff<LK, LV>>,
    )| {
        let mut scratch = std::mem::take(&mut state.scratch);
        for diff in diffs {
            apply_left_diff(state, diff, left_join_key, &mut scratch.impacted);
        }
        let changes = recompute_keyed_impacted(state, &mut scratch, &|key, value, rights| {
            Some(compute_value(key, value, rights))
        });
        state.scratch = scratch;
        changes
    };
    process_join_shards(&mut join.shards, routed, process)
}

fn process_right_shards<LK, LV, RK, RV, JK, OV, FR, FO>(
    join: &mut ShardedKeyedJoin<LK, LV, RK, RV, JK, OV>,
    routed: &[Vec<MapDiff<RK, RV>>],
    right_join_key: &FR,
    compute_value: &FO,
) -> Vec<Vec<MapDiff<LK, OV>>>
where
    LK: Hash + Eq + CellValue,
    LV: CellValue,
    RK: Hash + Eq + CellValue,
    RV: CellValue,
    JK: Hash + Eq + CellValue,
    OV: CellValue,
    FR: Fn(&RK, &RV) -> JK + Sync,
    FO: Fn(&LK, &LV, &[(RK, RV)]) -> OV + Sync,
{
    let process = |(state, diffs): (
        &mut JoinState<LK, LV, RK, RV, JK, LK, OV>,
        &Vec<MapDiff<RK, RV>>,
    )| {
        let mut scratch = std::mem::take(&mut state.scratch);
        for diff in diffs {
            apply_right_diff(
                state,
                diff,
                right_join_key,
                &mut scratch.impacted,
                &mut scratch.changed_join_keys,
            );
        }
        let changes = recompute_keyed_impacted(state, &mut scratch, &|key, value, rights| {
            Some(compute_value(key, value, rights))
        });
        state.scratch = scratch;
        changes
    };
    process_join_shards(&mut join.shards, routed, process)
}

fn process_join_shards<State, Diff, Change, F>(
    shards: &mut [State],
    routed: &[Vec<Diff>],
    process: F,
) -> Vec<Vec<Change>>
where
    State: Send,
    Diff: Sync,
    Change: Send,
    F: Fn((&mut State, &Vec<Diff>)) -> Vec<Change> + Send + Sync,
{
    let work = routed
        .iter()
        .fold(0_usize, |sum, diffs| sum.saturating_add(diffs.len()));
    #[cfg(all(feature = "scheduler", not(target_arch = "wasm32")))]
    if work >= 8_192
        && shards.len() > 1
        && let Some(pool) = crate::executor::worker_pool()
    {
        use rayon::prelude::*;
        return pool.install(|| {
            shards
                .par_iter_mut()
                .zip(routed.par_iter())
                .map(&process)
                .collect()
        });
    }

    let _ = work;
    shards.iter_mut().zip(routed).map(process).collect()
}

fn diff_work<K, V>(diff: &MapDiff<K, V>) -> usize {
    match diff {
        MapDiff::Initial { entries } => entries.len(),
        MapDiff::Batch { changes } => changes.iter().fold(0_usize, |work, change| {
            work.saturating_add(diff_work(change))
        }),
        MapDiff::Insert { .. } | MapDiff::Update { .. } | MapDiff::Remove { .. } => 1,
    }
}

fn state_left_entries<LK, LV, RK, RV, JK, OK, OV>(
    state: &JoinState<LK, LV, RK, RV, JK, OK, OV, impl RelationIndexStorage<RK, RV, JK>>,
) -> Vec<(LK, LV)>
where
    LK: Hash + Eq + CellValue,
    LV: CellValue,
    RK: Hash + Eq + CellValue,
    RV: CellValue,
    JK: Hash + Eq + CellValue,
    OK: Hash + Eq + CellValue,
    OV: CellValue,
{
    state
        .join_to_left
        .values()
        .flatten()
        .filter_map(|key| {
            state
                .left_rows
                .get(key)
                .map(|value| (key.clone(), value.clone()))
        })
        .collect()
}

fn state_right_entries<LK, LV, RK, RV, JK, OK, OV>(
    state: &JoinState<LK, LV, RK, RV, JK, OK, OV, impl RelationIndexStorage<RK, RV, JK>>,
) -> Vec<(RK, RV)>
where
    LK: Hash + Eq + CellValue,
    LV: CellValue,
    RK: Hash + Eq + CellValue,
    RV: CellValue,
    JK: Hash + Eq + CellValue,
    OK: Hash + Eq + CellValue,
    OV: CellValue,
{
    state.right.read(|right| {
        right
            .join_to_rows
            .values()
            .flatten()
            .filter_map(|key| {
                right
                    .rows
                    .get(key)
                    .map(|value| (key.clone(), value.clone()))
            })
            .collect()
    })
}

/// Install the zero-or-one-output, left-key-preserving join runtime.
pub fn install_keyed_join_runtime_via_query<LK, LV, RK, RV, JK, OV, L, R, FL, FR, FO, Sink>(
    cx: &mut crate::map_query::compiler::CompileContext,
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
    let relation = cx.take_relation_hint();
    if let Some(relation) = relation {
        let index = cx.prepare_relationship_index::<RelationIndex<RK, RV, JK>>();
        return install_keyed_join_runtime_with_index(
            cx,
            index.clone(),
            Some((relation, index)),
            left,
            right,
            left_join_key,
            right_join_key,
            compute_value,
            sink,
        );
    }

    install_keyed_join_runtime_with_index(
        cx,
        RelationIndex::default(),
        None,
        left,
        right,
        left_join_key,
        right_join_key,
        compute_value,
        sink,
    )
}

#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn install_keyed_join_runtime_with_index<LK, LV, RK, RV, JK, OV, L, R, FL, FR, FO, Sink, RI>(
    cx: &mut crate::map_query::compiler::CompileContext,
    right_index: RI,
    relationship_binding: Option<(
        TypeId,
        crate::map_query::compiler::DeferredPhysical<RelationIndex<RK, RV, JK>>,
    )>,
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
    RI: RelationIndexStorage<RK, RV, JK>,
{
    let state = Arc::new(Mutex::new(
        JoinState::<LK, LV, RK, RV, JK, LK, OV, RI>::with_right(right_index),
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

    let mut guards = left.compile_into(cx, left_sink);
    if let Some((relation, index)) = relationship_binding {
        guards.extend(
            cx.with_root_relation_index(relation, index, |cx| right.compile_into(cx, right_sink)),
        );
    } else {
        guards.extend(right.compile_into(cx, right_sink));
    }
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
    cx: &mut crate::map_query::compiler::CompileContext,
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
    type FirstShards<LK, LV, RK, RV, JK, MV> = ShardedKeyedJoin<LK, LV, RK, RV, JK, MV>;
    type SecondShards<LK, MV, RK, RV, JK, OV> = ShardedKeyedJoin<LK, MV, RK, RV, JK, OV>;

    let shard_count = query_shard_count();
    let state = Arc::new(Mutex::new((
        FirstState::<LK, LV, RK1, RV1, JK1, MV>::default(),
        SecondState::<LK, MV, RK2, RV2, JK2, OV>::default(),
        None::<(
            FirstShards<LK, LV, RK1, RV1, JK1, MV>,
            SecondShards<LK, MV, RK2, RV2, JK2, OV>,
        )>,
    )));
    let left_join_key1 = Arc::new(left_join_key1);
    let right_join_key1 = Arc::new(right_join_key1);
    let map_first = Arc::new(map_first);
    let left_join_key2 = Arc::new(left_join_key2);
    let right_join_key2 = Arc::new(right_join_key2);
    let map_second = Arc::new(map_second);
    let sink = Arc::new(sink);

    let propagate_sequential = {
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
    let propagate_sequential = Arc::new(propagate_sequential);

    let propagate_sharded = {
        let left_join_key2 = Arc::clone(&left_join_key2);
        let map_second = Arc::clone(&map_second);
        move |first: &mut FirstShards<LK, LV, RK1, RV1, JK1, MV>,
              second: &mut SecondShards<LK, MV, RK2, RV2, JK2, OV>,
              first_shard_changes: Vec<Vec<MapDiff<LK, MV>>>,
              first_event_ordinals: &FxHashMap<LK, u64>| {
            let intermediate = first.merge_changes(first_shard_changes, first_event_ordinals);
            let (routed_second, second_event_ordinals) =
                second.route_left_owned(intermediate, left_join_key2.as_ref());
            let second_shard_changes = process_left_shards(
                second,
                &routed_second,
                left_join_key2.as_ref(),
                map_second.as_ref(),
            );
            second.merge_changes(second_shard_changes, &second_event_ordinals)
        }
    };
    let propagate_sharded = Arc::new(propagate_sharded);

    let promote = {
        let left_join_key1 = Arc::clone(&left_join_key1);
        let right_join_key1 = Arc::clone(&right_join_key1);
        let map_first = Arc::clone(&map_first);
        let left_join_key2 = Arc::clone(&left_join_key2);
        let right_join_key2 = Arc::clone(&right_join_key2);
        let map_second = Arc::clone(&map_second);
        move |first: &FirstState<LK, LV, RK1, RV1, JK1, MV>,
              second: &SecondState<LK, MV, RK2, RV2, JK2, OV>| {
            let mut first_shards = FirstShards::new(shard_count);
            let first_left = MapDiff::Initial {
                entries: state_left_entries(first),
            };
            let (routed, _) = first_shards.route_left(&first_left, left_join_key1.as_ref());
            let _ = process_left_shards(
                &mut first_shards,
                &routed,
                left_join_key1.as_ref(),
                map_first.as_ref(),
            );
            let first_right = MapDiff::Initial {
                entries: state_right_entries(first),
            };
            let routed = first_shards.route_right(&first_right, right_join_key1.as_ref());
            let _ = process_right_shards(
                &mut first_shards,
                &routed,
                right_join_key1.as_ref(),
                map_first.as_ref(),
            );

            let mut second_shards = SecondShards::new(shard_count);
            let second_left = MapDiff::Initial {
                entries: state_left_entries(second),
            };
            let (routed, _) = second_shards.route_left(&second_left, left_join_key2.as_ref());
            let _ = process_left_shards(
                &mut second_shards,
                &routed,
                left_join_key2.as_ref(),
                map_second.as_ref(),
            );
            let second_right = MapDiff::Initial {
                entries: state_right_entries(second),
            };
            let routed = second_shards.route_right(&second_right, right_join_key2.as_ref());
            let _ = process_right_shards(
                &mut second_shards,
                &routed,
                right_join_key2.as_ref(),
                map_second.as_ref(),
            );
            (first_shards, second_shards)
        }
    };
    let promote = Arc::new(promote);

    let left_sink = {
        let state = Arc::clone(&state);
        let left_join_key1 = Arc::clone(&left_join_key1);
        let map_first = Arc::clone(&map_first);
        let propagate_sequential = Arc::clone(&propagate_sequential);
        let propagate_sharded = Arc::clone(&propagate_sharded);
        let promote = Arc::clone(&promote);
        let sink = Arc::clone(&sink);
        move |diff: &MapDiff<LK, LV>| {
            let changes = {
                let mut state = state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if state.2.is_none() && shard_count > 1 && diff_work(diff) >= 8_192 {
                    let shards = promote(&state.0, &state.1);
                    state.2 = Some(shards);
                }
                let changes = if let Some((first, second)) = &mut state.2 {
                    let (routed, event_ordinals) = first.route_left(diff, left_join_key1.as_ref());
                    let shard_changes = process_left_shards(
                        first,
                        &routed,
                        left_join_key1.as_ref(),
                        map_first.as_ref(),
                    );
                    propagate_sharded(first, second, shard_changes, &event_ordinals)
                } else {
                    let (first, second, _) = &mut *state;
                    let mut scratch = std::mem::take(&mut first.scratch);
                    apply_left_diff(first, diff, left_join_key1.as_ref(), &mut scratch.impacted);
                    first.scratch = scratch;
                    propagate_sequential(first, second)
                };
                drop(state);
                changes
            };
            emit_changes(sink.as_ref(), changes);
        }
    };

    let right1_sink = {
        let state = Arc::clone(&state);
        let right_join_key1 = Arc::clone(&right_join_key1);
        let map_first = Arc::clone(&map_first);
        let propagate_sequential = Arc::clone(&propagate_sequential);
        let propagate_sharded = Arc::clone(&propagate_sharded);
        let promote = Arc::clone(&promote);
        let sink = Arc::clone(&sink);
        move |diff: &MapDiff<RK1, RV1>| {
            let changes = {
                let mut state = state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if state.2.is_none() && shard_count > 1 && diff_work(diff) >= 8_192 {
                    let shards = promote(&state.0, &state.1);
                    state.2 = Some(shards);
                }
                let changes = if let Some((first, second)) = &mut state.2 {
                    let routed = first.route_right(diff, right_join_key1.as_ref());
                    let shard_changes = process_right_shards(
                        first,
                        &routed,
                        right_join_key1.as_ref(),
                        map_first.as_ref(),
                    );
                    propagate_sharded(first, second, shard_changes, &FxHashMap::default())
                } else {
                    let (first, second, _) = &mut *state;
                    let mut scratch = std::mem::take(&mut first.scratch);
                    apply_right_diff(
                        first,
                        diff,
                        right_join_key1.as_ref(),
                        &mut scratch.impacted,
                        &mut scratch.changed_join_keys,
                    );
                    first.scratch = scratch;
                    propagate_sequential(first, second)
                };
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
        let promote = promote;
        let sink = sink;
        move |diff: &MapDiff<RK2, RV2>| {
            let changes = {
                let mut state = state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if state.2.is_none() && shard_count > 1 && diff_work(diff) >= 8_192 {
                    let shards = promote(&state.0, &state.1);
                    state.2 = Some(shards);
                }
                let changes = if let Some((_, second)) = &mut state.2 {
                    let routed = second.route_right(diff, right_join_key2.as_ref());
                    let shard_changes = process_right_shards(
                        second,
                        &routed,
                        right_join_key2.as_ref(),
                        map_second.as_ref(),
                    );
                    second.merge_changes(shard_changes, &FxHashMap::default())
                } else {
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
                    changes
                };
                drop(state);
                changes
            };
            emit_changes(sink.as_ref(), changes);
        }
    };

    let mut guards = left.compile_into(cx, left_sink);
    guards.extend(right1.compile_into(cx, right1_sink));
    guards.extend(right2.compile_into(cx, right2_sink));
    guards
}
