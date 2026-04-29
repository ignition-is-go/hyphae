use std::{
    collections::{HashMap, HashSet},
    hash::Hash,
    sync::{Arc, Mutex},
};

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
    left_rows: HashMap<LK, LV>,
    /// Each left item maps to multiple join keys.
    left_join_keys: HashMap<LK, Vec<JK>>,
    /// Reverse index: join key -> set of left keys that reference it.
    join_to_left: HashMap<JK, HashSet<LK>>,
    right_rows: HashMap<RK, RV>,
    right_join_keys: HashMap<RK, JK>,
    join_to_right: HashMap<JK, HashSet<RK>>,
    left_output_keys: HashMap<LK, HashSet<OK>>,
    output_cache: HashMap<OK, OV>,
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
            left_rows: HashMap::new(),
            left_join_keys: HashMap::new(),
            join_to_left: HashMap::new(),
            right_rows: HashMap::new(),
            right_join_keys: HashMap::new(),
            join_to_right: HashMap::new(),
            left_output_keys: HashMap::new(),
            output_cache: HashMap::new(),
        }
    }
}

fn add_index_member<I, M>(index: &mut HashMap<I, HashSet<M>>, index_key: I, member: M)
where
    I: Hash + Eq + CellValue,
    M: Hash + Eq + CellValue,
{
    index.entry(index_key).or_default().insert(member);
}

fn remove_index_member<I, M>(index: &mut HashMap<I, HashSet<M>>, index_key: &I, member: &M)
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
    impacted: &mut HashSet<LK>,
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
    impacted: &mut HashSet<LK>,
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
    if state.left_rows.remove(left_key).is_some() || state.left_output_keys.contains_key(left_key) {
        impacted.insert(left_key.clone());
    }
}

fn apply_left_diff<LK, LV, RK, RV, JK, OK, OV, FL>(
    state: &mut MultiJoinState<LK, LV, RK, RV, JK, OK, OV>,
    diff: &MapDiff<LK, LV>,
    left_join_keys_fn: &FL,
    impacted: &mut HashSet<LK>,
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
    changed_join_keys: &mut HashSet<JK>,
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
    changed_join_keys: &mut HashSet<JK>,
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
    impacted: &mut HashSet<LK>,
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
    let mut changed_join_keys: HashSet<JK> = HashSet::new();

    fn apply_one<LK, LV, RK, RV, JK, OK, OV, FR>(
        state: &mut MultiJoinState<LK, LV, RK, RV, JK, OK, OV>,
        diff: &MapDiff<RK, RV>,
        right_join_key: &FR,
        changed_join_keys: &mut HashSet<JK>,
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

    apply_one(state, diff, right_join_key, &mut changed_join_keys);

    for join_key in changed_join_keys {
        if let Some(left_keys) = state.join_to_left.get(&join_key) {
            impacted.extend(left_keys.iter().cloned());
        }
    }
}

/// Recompute output for all impacted left keys.
///
/// The key difference from `join_runtime::recompute_impacted`: right rows are
/// collected across ALL join keys for a given left item (union), not just one.
fn recompute_impacted<LK, LV, RK, RV, JK, OK, OV, FO>(
    state: &mut MultiJoinState<LK, LV, RK, RV, JK, OK, OV>,
    impacted: HashSet<LK>,
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

    for left_key in impacted {
        let previous_output_keys = state.left_output_keys.remove(&left_key).unwrap_or_default();

        let mut desired_rows: HashMap<OK, OV> = HashMap::new();
        if let Some(left_value) = state.left_rows.get(&left_key) {
            // Collect right rows across ALL join keys for this left item.
            let mut seen_right_keys = HashSet::new();
            let mut right_rows: Vec<(RK, RV)> = Vec::new();

            if let Some(join_keys) = state.left_join_keys.get(&left_key) {
                for jk in join_keys {
                    if let Some(right_key_set) = state.join_to_right.get(jk) {
                        for rk in right_key_set {
                            if seen_right_keys.insert(rk.clone())
                                && let Some(rv) = state.right_rows.get(rk)
                            {
                                right_rows.push((rk.clone(), rv.clone()));
                            }
                        }
                    }
                }
            }

            for (output_key, output_value) in compute_rows(&left_key, left_value, &right_rows) {
                desired_rows.insert(output_key, output_value);
            }
        }

        let desired_keys: HashSet<OK> = desired_rows.keys().cloned().collect();

        for stale_key in previous_output_keys
            .iter()
            .filter(|output_key| !desired_keys.contains(*output_key))
        {
            if let Some(old_value) = state.output_cache.remove(stale_key) {
                changes.push(MapDiff::Remove {
                    key: stale_key.clone(),
                    old_value,
                });
            }
        }

        for (output_key, new_value) in desired_rows {
            match state.output_cache.get(&output_key).cloned() {
                Some(old_value) => {
                    if old_value != new_value {
                        state
                            .output_cache
                            .insert(output_key.clone(), new_value.clone());
                        changes.push(MapDiff::Update {
                            key: output_key,
                            old_value,
                            new_value,
                        });
                    }
                }
                None => {
                    state
                        .output_cache
                        .insert(output_key.clone(), new_value.clone());
                    changes.push(MapDiff::Insert {
                        key: output_key,
                        value: new_value,
                    });
                }
            }
        }

        if !desired_keys.is_empty() {
            state.left_output_keys.insert(left_key, desired_keys);
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
fn emit_changes<OK, OV>(sink: &crate::map_query::MapDiffSink<OK, OV>, changes: Vec<MapDiff<OK, OV>>)
where
    OK: Hash + Eq + CellValue,
    OV: CellValue,
{
    if changes.is_empty() {
        return;
    }
    sink(&MapDiff::Batch { changes });
}

/// Install multi-join machinery that drives `sink` instead of allocating an
/// output map.
///
/// Subscribes to both source maps via
/// [`MapQuery::install`](crate::map_query::MapQuery::install), maintains the
/// multi-join state, and pushes resulting `MapDiff`s into the sink. Returns
/// the subscription guards (caller owns them — typically attaches them to the
/// materialized output). Chains of plans compose without intermediate
/// [`CellMap`](crate::CellMap) allocations.
///
/// Used by `MapQuery` multi-join plan nodes whose materialization owns a
/// single output cell map; multiple plan stages share that output rather than
/// each allocating their own.
pub(crate) fn install_multi_join_runtime_via_query<LK, LV, RK, RV, JK, OK, OV, L, R, FL, FR, FO>(
    left: L,
    right: R,
    left_join_keys_fn: FL,
    right_join_key: FR,
    compute_rows: FO,
    sink: crate::map_query::MapDiffSink<OK, OV>,
) -> Vec<SubscriptionGuard>
where
    LK: Hash + Eq + CellValue,
    LV: CellValue,
    RK: Hash + Eq + CellValue,
    RV: CellValue,
    JK: Hash + Eq + CellValue,
    OK: Hash + Eq + CellValue,
    OV: CellValue,
    L: crate::map_query::MapQuery<LK, LV>,
    R: crate::map_query::MapQuery<RK, RV>,
    FL: Fn(&LK, &LV) -> Vec<JK> + Send + Sync + 'static,
    FR: Fn(&RK, &RV) -> JK + Send + Sync + 'static,
    FO: Fn(&LK, &LV, &[(RK, RV)]) -> Vec<(OK, OV)> + Send + Sync + 'static,
{
    let state = Arc::new(Mutex::new(
        MultiJoinState::<LK, LV, RK, RV, JK, OK, OV>::default(),
    ));
    let left_join_keys_fn = Arc::new(left_join_keys_fn);
    let right_join_key = Arc::new(right_join_key);
    let compute_rows = Arc::new(compute_rows);

    let left_sink: crate::map_query::MapDiffSink<LK, LV> = {
        let state = state.clone();
        let left_join_keys_fn = left_join_keys_fn.clone();
        let compute_rows = compute_rows.clone();
        let sink = sink.clone();
        Arc::new(move |diff| {
            let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
            let mut impacted: HashSet<LK> = HashSet::new();
            apply_left_diff(&mut state, diff, left_join_keys_fn.as_ref(), &mut impacted);
            let changes = recompute_impacted(&mut state, impacted, compute_rows.as_ref());
            drop(state);
            emit_changes(&sink, changes);
        })
    };

    let right_sink: crate::map_query::MapDiffSink<RK, RV> = {
        let state = state.clone();
        let right_join_key = right_join_key.clone();
        let compute_rows = compute_rows.clone();
        let sink = sink.clone();
        Arc::new(move |diff| {
            let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
            let mut impacted: HashSet<LK> = HashSet::new();
            apply_right_diff(&mut state, diff, right_join_key.as_ref(), &mut impacted);
            let changes = recompute_impacted(&mut state, impacted, compute_rows.as_ref());
            drop(state);
            emit_changes(&sink, changes);
        })
    };

    let mut guards = left.install(left_sink);
    guards.extend(right.install(right_sink));
    guards
}
