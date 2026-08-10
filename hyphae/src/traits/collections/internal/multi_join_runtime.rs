use std::{
    collections::hash_map::Entry,
    hash::Hash,
    sync::{Arc, Mutex},
};

use rustc_hash::{FxHashMap, FxHashSet};

use crate::{cell_map::MapDiff, subscription::SubscriptionGuard, traits::CellValue};

struct MultiJoinState<LK, LV, RK, RV, JK, OK, OV>
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
    /// Each left item maps to multiple join keys.
    left_join_keys: FxHashMap<LK, Vec<JK>>,
    /// Reverse index: join key -> set of left keys that reference it.
    join_to_left: FxHashMap<JK, FxHashSet<LK>>,
    right_rows: FxHashMap<RK, RV>,
    right_join_keys: FxHashMap<RK, JK>,
    join_to_right: FxHashMap<JK, FxHashSet<RK>>,
    output_cache: FxHashMap<OK, OV>,
    scratch: MultiJoinScratch<LK, RK, RV, JK>,
}

struct MultiJoinScratch<LK, RK, RV, JK> {
    impacted: FxHashSet<LK>,
    changed_join_keys: FxHashSet<JK>,
    seen_right_keys: FxHashSet<RK>,
    right_rows: Vec<(RK, RV)>,
}

impl<LK, RK, RV, JK> Default for MultiJoinScratch<LK, RK, RV, JK> {
    fn default() -> Self {
        Self {
            impacted: FxHashSet::default(),
            changed_join_keys: FxHashSet::default(),
            seen_right_keys: FxHashSet::default(),
            right_rows: Vec::new(),
        }
    }
}

impl<LK, LV, RK, RV, JK, OK, OV> Default for MultiJoinState<LK, LV, RK, RV, JK, OK, OV>
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
            output_cache: FxHashMap::default(),
            scratch: MultiJoinScratch::default(),
        }
    }
}

fn add_index_member<I, M>(index: &mut FxHashMap<I, FxHashSet<M>>, index_key: I, member: M)
where
    I: Hash + Eq + CellValue,
    M: Hash + Eq + CellValue,
{
    index.entry(index_key).or_default().insert(member);
}

fn remove_index_member<I, M>(index: &mut FxHashMap<I, FxHashSet<M>>, index_key: &I, member: &M)
where
    I: Hash + Eq + CellValue,
    M: Hash + Eq + CellValue,
{
    if let Some(members) = index.get_mut(index_key) {
        members.remove(member);
        if members.is_empty() {
            index.remove(index_key);
        }
    }
}

fn upsert_left<LK, LV, RK, RV, JK, OK, OV, FL>(
    state: &mut MultiJoinState<LK, LV, RK, RV, JK, OK, OV>,
    left_key: LK,
    left_value: LV,
    left_join_keys_fn: &FL,
    impacted: &mut FxHashSet<LK>,
) where
    LK: Hash + Eq + CellValue,
    LV: CellValue,
    RK: Hash + Eq + CellValue,
    RV: CellValue,
    JK: Hash + Eq + CellValue,
    OK: Hash + Eq + CellValue,
    OV: CellValue,
    FL: Fn(&LK, &LV) -> Vec<JK>,
{
    // Remove previous join key mappings
    if let Some(previous_join_keys) = state.left_join_keys.remove(&left_key) {
        for jk in &previous_join_keys {
            remove_index_member(&mut state.join_to_left, jk, &left_key);
        }
    }

    // Compute new join keys
    let join_keys = left_join_keys_fn(&left_key, &left_value);
    state.left_rows.insert(left_key.clone(), left_value);

    // Add to reverse index for each join key
    for jk in &join_keys {
        add_index_member(&mut state.join_to_left, jk.clone(), left_key.clone());
    }
    state.left_join_keys.insert(left_key.clone(), join_keys);
    impacted.insert(left_key);
}

fn remove_left<LK, LV, RK, RV, JK, OK, OV>(
    state: &mut MultiJoinState<LK, LV, RK, RV, JK, OK, OV>,
    left_key: &LK,
    impacted: &mut FxHashSet<LK>,
) where
    LK: Hash + Eq + CellValue,
    LV: CellValue,
    RK: Hash + Eq + CellValue,
    RV: CellValue,
    JK: Hash + Eq + CellValue,
    OK: Hash + Eq + CellValue,
    OV: CellValue,
{
    if let Some(previous_join_keys) = state.left_join_keys.remove(left_key) {
        for jk in &previous_join_keys {
            remove_index_member(&mut state.join_to_left, jk, left_key);
        }
    }
    if state.left_rows.remove(left_key).is_some() {
        impacted.insert(left_key.clone());
    }
}

fn apply_left_diff<LK, LV, RK, RV, JK, OK, OV, FL>(
    state: &mut MultiJoinState<LK, LV, RK, RV, JK, OK, OV>,
    diff: &MapDiff<LK, LV>,
    left_join_keys_fn: &FL,
    impacted: &mut FxHashSet<LK>,
) where
    LK: Hash + Eq + CellValue,
    LV: CellValue,
    RK: Hash + Eq + CellValue,
    RV: CellValue,
    JK: Hash + Eq + CellValue,
    OK: Hash + Eq + CellValue,
    OV: CellValue,
    FL: Fn(&LK, &LV) -> Vec<JK>,
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
                upsert_left(
                    state,
                    key.clone(),
                    value.clone(),
                    left_join_keys_fn,
                    impacted,
                );
            }
        }
        MapDiff::Insert { key, value }
        | MapDiff::Update {
            key,
            new_value: value,
            ..
        } => {
            upsert_left(
                state,
                key.clone(),
                value.clone(),
                left_join_keys_fn,
                impacted,
            );
        }
        MapDiff::Remove { key, .. } => {
            remove_left(state, key, impacted);
        }
        MapDiff::Batch { changes } => {
            for change in changes {
                apply_left_diff(state, change, left_join_keys_fn, impacted);
            }
        }
    }
}

// Right side is identical to join_runtime — single key per right item.
fn upsert_right<LK, LV, RK, RV, JK, OK, OV, FR>(
    state: &mut MultiJoinState<LK, LV, RK, RV, JK, OK, OV>,
    right_key: RK,
    right_value: RV,
    right_join_key: &FR,
    changed_join_keys: &mut FxHashSet<JK>,
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
    if let Some(previous_join_key) = state.right_join_keys.remove(&right_key) {
        remove_index_member(&mut state.join_to_right, &previous_join_key, &right_key);
        changed_join_keys.insert(previous_join_key);
    }

    let join_key = right_join_key(&right_key, &right_value);
    state.right_rows.insert(right_key.clone(), right_value);
    state
        .right_join_keys
        .insert(right_key.clone(), join_key.clone());
    add_index_member(&mut state.join_to_right, join_key.clone(), right_key);
    changed_join_keys.insert(join_key);
}

fn remove_right<LK, LV, RK, RV, JK, OK, OV>(
    state: &mut MultiJoinState<LK, LV, RK, RV, JK, OK, OV>,
    right_key: &RK,
    changed_join_keys: &mut FxHashSet<JK>,
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
    state: &mut MultiJoinState<LK, LV, RK, RV, JK, OK, OV>,
    diff: &MapDiff<RK, RV>,
    right_join_key: &FR,
    impacted: &mut FxHashSet<LK>,
    changed_join_keys: &mut FxHashSet<JK>,
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
        state: &mut MultiJoinState<LK, LV, RK, RV, JK, OK, OV>,
        diff: &MapDiff<RK, RV>,
        right_join_key: &FR,
        changed_join_keys: &mut FxHashSet<JK>,
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

fn recompute_keyed_impacted<LK, LV, RK, RV, JK, OV, FO>(
    state: &mut MultiJoinState<LK, LV, RK, RV, JK, LK, OV>,
    scratch: &mut MultiJoinScratch<LK, RK, RV, JK>,
    compute_value: &FO,
) -> Vec<MapDiff<LK, OV>>
where
    LK: Hash + Eq + CellValue,
    LV: CellValue,
    RK: Hash + Eq + CellValue,
    RV: CellValue,
    JK: Hash + Eq + CellValue,
    OV: CellValue,
    FO: Fn(&LK, &LV, &[(RK, RV)]) -> OV,
{
    let mut changes = Vec::new();

    for left_key in scratch.impacted.drain() {
        scratch.seen_right_keys.clear();
        scratch.right_rows.clear();
        let desired_value = state.left_rows.get(&left_key).map(|left_value| {
            if let Some(join_keys) = state.left_join_keys.get(&left_key) {
                for join_key in join_keys {
                    if let Some(right_keys) = state.join_to_right.get(join_key) {
                        for right_key in right_keys {
                            if scratch.seen_right_keys.insert(right_key.clone())
                                && let Some(right_value) = state.right_rows.get(right_key)
                            {
                                scratch
                                    .right_rows
                                    .push((right_key.clone(), right_value.clone()));
                            }
                        }
                    }
                }
            }
            compute_value(&left_key, left_value, &scratch.right_rows)
        });

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

    changes
}

// ── The public entry points ─────────────────────────────────────────────

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

/// Install the one-output, left-key-preserving multi-join runtime.
pub fn install_keyed_multi_join_runtime_via_query<LK, LV, RK, RV, JK, OV, L, R, FL, FR, FO, Sink>(
    left: L,
    right: R,
    left_join_keys_fn: FL,
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
    FL: Fn(&LK, &LV) -> Vec<JK> + Send + Sync + 'static,
    FR: Fn(&RK, &RV) -> JK + Send + Sync + 'static,
    FO: Fn(&LK, &LV, &[(RK, RV)]) -> OV + Send + Sync + 'static,
    Sink: crate::map_query::MapDiffSink<LK, OV>,
{
    let state = Arc::new(Mutex::new(
        MultiJoinState::<LK, LV, RK, RV, JK, LK, OV>::default(),
    ));
    let left_join_keys_fn = Arc::new(left_join_keys_fn);
    let right_join_key = Arc::new(right_join_key);
    let compute_value = Arc::new(compute_value);
    let sink = Arc::new(sink);

    let left_sink = {
        let state = state.clone();
        let left_join_keys_fn = left_join_keys_fn;
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
                left_join_keys_fn.as_ref(),
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
