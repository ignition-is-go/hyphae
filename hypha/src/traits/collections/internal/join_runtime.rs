use std::{
    collections::{HashMap, HashSet},
    hash::Hash,
    marker::PhantomData,
    sync::Arc,
};

use arc_swap::ArcSwap;

use crate::{
    cell::{CellImmutable, CellMutable},
    cell_map::{CellMap, MapDiff},
    traits::CellValue,
};

#[derive(Clone)]
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
    left_rows: HashMap<LK, LV>,
    left_join_keys: HashMap<LK, JK>,
    join_to_left: HashMap<JK, HashSet<LK>>,
    right_rows: HashMap<RK, RV>,
    right_join_keys: HashMap<RK, JK>,
    join_to_right: HashMap<JK, HashSet<RK>>,
    left_output_keys: HashMap<LK, HashSet<OK>>,
    output_cache: HashMap<OK, OV>,
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
    state: &mut JoinState<LK, LV, RK, RV, JK, OK, OV>,
    left_key: LK,
    left_value: LV,
    left_join_key: &FL,
    impacted: &mut HashSet<LK>,
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
    if let Some(previous_join_key) = state.left_join_keys.remove(&left_key) {
        remove_index_member(&mut state.join_to_left, &previous_join_key, &left_key);
    }

    let join_key = left_join_key(&left_key, &left_value);
    state.left_rows.insert(left_key.clone(), left_value);
    state
        .left_join_keys
        .insert(left_key.clone(), join_key.clone());
    add_index_member(&mut state.join_to_left, join_key, left_key.clone());
    impacted.insert(left_key);
}

fn remove_left<LK, LV, RK, RV, JK, OK, OV>(
    state: &mut JoinState<LK, LV, RK, RV, JK, OK, OV>,
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
    impacted: &mut HashSet<LK>,
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
    state: &mut JoinState<LK, LV, RK, RV, JK, OK, OV>,
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
    state: &mut JoinState<LK, LV, RK, RV, JK, OK, OV>,
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
        state: &mut JoinState<LK, LV, RK, RV, JK, OK, OV>,
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

fn recompute_impacted<LK, LV, RK, RV, JK, OK, OV, FO>(
    state: &mut JoinState<LK, LV, RK, RV, JK, OK, OV>,
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
            let right_rows: Vec<(RK, RV)> = state
                .left_join_keys
                .get(&left_key)
                .and_then(|join_key| state.join_to_right.get(join_key))
                .map(|right_keys| {
                    right_keys
                        .iter()
                        .filter_map(|right_key| {
                            state
                                .right_rows
                                .get(right_key)
                                .map(|right_value| (right_key.clone(), right_value.clone()))
                        })
                        .collect()
                })
                .unwrap_or_default();

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
                    state
                        .output_cache
                        .insert(output_key.clone(), new_value.clone());
                    changes.push(MapDiff::Update {
                        key: output_key,
                        old_value,
                        new_value,
                    });
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

pub(crate) fn run_join_runtime<LK, LV, LM, RK, RV, RM, JK, OK, OV, FL, FR, FO>(
    left: &CellMap<LK, LV, LM>,
    right: &CellMap<RK, RV, RM>,
    op_name: &str,
    left_join_key: FL,
    right_join_key: FR,
    compute_rows: FO,
) -> CellMap<OK, OV, CellImmutable>
where
    LK: Hash + Eq + CellValue,
    LV: CellValue,
    RK: Hash + Eq + CellValue,
    RV: CellValue,
    JK: Hash + Eq + CellValue,
    OK: Hash + Eq + CellValue,
    OV: CellValue,
    FL: Fn(&LK, &LV) -> JK + Send + Sync + 'static,
    FR: Fn(&RK, &RV) -> JK + Send + Sync + 'static,
    FO: Fn(&LK, &LV, &[(RK, RV)]) -> Vec<(OK, OV)> + Send + Sync + 'static,
{
    let output = CellMap::<OK, OV, CellMutable>::new();
    if let Some(parent_name) = (**left.inner.name.load()).as_ref() {
        output
            .clone()
            .with_name(format!("{}::{}", parent_name, op_name));
    }

    let state = Arc::new(ArcSwap::from_pointee(
        JoinState::<LK, LV, RK, RV, JK, OK, OV>::default(),
    ));
    let left_join_key = Arc::new(left_join_key);
    let right_join_key = Arc::new(right_join_key);
    let compute_rows = Arc::new(compute_rows);

    let output_weak = Arc::downgrade(&output.inner);
    let left_guard = left.subscribe_diffs({
        let state = state.clone();
        let left_join_key = left_join_key.clone();
        let compute_rows = compute_rows.clone();
        move |diff| {
            let Some(inner) = output_weak.upgrade() else {
                return;
            };
            let output = CellMap::<OK, OV, CellMutable> {
                inner,
                _marker: PhantomData,
            };

            let changes_cell = std::cell::RefCell::new(Vec::<MapDiff<OK, OV>>::new());
            state.rcu(|current| {
                let mut next = current.as_ref().clone();
                let mut impacted: HashSet<LK> = HashSet::new();
                apply_left_diff(&mut next, diff, left_join_key.as_ref(), &mut impacted);
                let changes = recompute_impacted(&mut next, impacted, compute_rows.as_ref());
                *changes_cell.borrow_mut() = changes;
                next
            });
            let changes = changes_cell.into_inner();
            output.apply_batch(changes);
        }
    });

    let output_weak = Arc::downgrade(&output.inner);
    let right_guard = right.subscribe_diffs({
        let state = state.clone();
        let right_join_key = right_join_key.clone();
        let compute_rows = compute_rows.clone();
        move |diff| {
            let Some(inner) = output_weak.upgrade() else {
                return;
            };
            let output = CellMap::<OK, OV, CellMutable> {
                inner,
                _marker: PhantomData,
            };

            let changes_cell = std::cell::RefCell::new(Vec::<MapDiff<OK, OV>>::new());
            state.rcu(|current| {
                let mut next = current.as_ref().clone();
                let mut impacted: HashSet<LK> = HashSet::new();
                apply_right_diff(&mut next, diff, right_join_key.as_ref(), &mut impacted);
                let changes = recompute_impacted(&mut next, impacted, compute_rows.as_ref());
                *changes_cell.borrow_mut() = changes;
                next
            });
            let changes = changes_cell.into_inner();
            output.apply_batch(changes);
        }
    });

    output.own(left_guard);
    output.own(right_guard);
    output.lock()
}
