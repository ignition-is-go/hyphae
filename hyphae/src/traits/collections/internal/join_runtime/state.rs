use std::{collections::hash_map::Entry, hash::Hash};

use rustc_hash::FxHashMap;

use crate::{
    cell_map::MapDiff,
    traits::{CellValue, RightJoinKey},
};

use super::super::ordered_set::OrderedSet;

pub(super) struct JoinState<LK, LV, RK, RV, JK, OK, OV, RI = RelationIndex<RK, RV, JK>>
where
    LK: Hash + Eq + CellValue,
    LV: CellValue,
    RK: Hash + Eq + CellValue,
    RV: CellValue,
    JK: Hash + Eq + CellValue,
    OK: Hash + Eq + CellValue,
    OV: CellValue,
{
    pub(super) left_rows: FxHashMap<LK, LV>,
    pub(super) left_join_keys: FxHashMap<LK, JK>,
    pub(super) join_to_left: FxHashMap<JK, Vec<LK>>,
    pub(super) right: RI,
    pub(super) left_output_keys: FxHashMap<LK, OrderedSet<OK>>,
    pub(super) output_cache: FxHashMap<OK, OV>,
    pub(super) parallel_active: bool,
    pub(super) scratch: JoinScratch<LK, RK, RV, JK, OK, OV>,
}

/// Typed physical index for one right-side relationship.
pub(in crate::traits::collections::internal) struct RelationIndex<RK, RV, JK> {
    pub(in crate::traits::collections::internal) rows: FxHashMap<RK, RV>,
    pub(in crate::traits::collections::internal) row_join_keys: FxHashMap<RK, JK>,
    pub(in crate::traits::collections::internal) join_to_rows: FxHashMap<JK, Vec<RK>>,
    pub(in crate::traits::collections::internal) grouped_rows: FxHashMap<JK, Vec<(RK, RV)>>,
}

impl<RK, RV, JK> Clone for RelationIndex<RK, RV, JK>
where
    RK: Clone,
    RV: Clone,
    JK: Clone,
{
    fn clone(&self) -> Self {
        Self {
            rows: self.rows.clone(),
            row_join_keys: self.row_join_keys.clone(),
            join_to_rows: self.join_to_rows.clone(),
            grouped_rows: self.grouped_rows.clone(),
        }
    }
}

impl<RK, RV, JK> Default for RelationIndex<RK, RV, JK> {
    fn default() -> Self {
        Self {
            rows: FxHashMap::default(),
            row_join_keys: FxHashMap::default(),
            join_to_rows: FxHashMap::default(),
            grouped_rows: FxHashMap::default(),
        }
    }
}

pub(in crate::traits::collections::internal) trait RelationIndexStorage<RK, RV, JK>:
    Send + Sync + 'static
{
    type Read<'a>: std::ops::Deref<Target = RelationIndex<RK, RV, JK>>
    where
        Self: 'a;

    fn acquire_read(&self) -> Self::Read<'_>;
    fn write<T>(&mut self, write: impl FnOnce(&mut RelationIndex<RK, RV, JK>) -> T) -> T;
}

impl<RK, RV, JK> RelationIndexStorage<RK, RV, JK> for RelationIndex<RK, RV, JK>
where
    RK: Send + Sync + 'static,
    RV: Send + Sync + 'static,
    JK: Send + Sync + 'static,
{
    type Read<'a>
        = &'a Self
    where
        Self: 'a;

    fn acquire_read(&self) -> Self::Read<'_> {
        self
    }

    fn write<T>(&mut self, write: impl FnOnce(&mut Self) -> T) -> T {
        write(self)
    }
}

#[allow(clippy::use_self)]
impl<RK, RV, JK> RelationIndexStorage<RK, RV, JK>
    for crate::map_query::compiler::DeferredPhysical<RelationIndex<RK, RV, JK>>
where
    RK: Send + Sync + 'static,
    RV: Send + Sync + 'static,
    JK: Send + Sync + 'static,
{
    type Read<'a>
        = parking_lot::RwLockReadGuard<'a, RelationIndex<RK, RV, JK>>
    where
        Self: 'a;

    fn acquire_read(&self) -> Self::Read<'_> {
        crate::map_query::compiler::DeferredPhysical::acquire_read(self)
    }

    fn write<T>(&mut self, write: impl FnOnce(&mut RelationIndex<RK, RV, JK>) -> T) -> T {
        crate::map_query::compiler::DeferredPhysical::write(self, write)
    }
}

fn remove_grouped_row<RK, RV, JK>(
    index: &mut RelationIndex<RK, RV, JK>,
    join_key: &JK,
    row_key: &RK,
) where
    RK: Eq,
    JK: Hash + Eq,
{
    if let Some(rows) = index.grouped_rows.get_mut(join_key) {
        rows.retain(|(key, _)| key != row_key);
        if rows.is_empty() {
            index.grouped_rows.remove(join_key);
        }
    }
}

fn upsert_grouped_row<RK, RV, JK>(
    index: &mut RelationIndex<RK, RV, JK>,
    join_key: JK,
    row_key: RK,
    row_value: RV,
) where
    RK: Eq,
    JK: Hash + Eq,
{
    let rows = index.grouped_rows.entry(join_key).or_default();
    if let Some((_, value)) = rows.iter_mut().find(|(key, _)| key == &row_key) {
        *value = row_value;
    } else {
        rows.push((row_key, row_value));
    }
}

pub(super) struct JoinScratch<LK, RK, RV, JK, OK, OV> {
    pub(super) impacted: OrderedSet<LK>,
    pub(super) impacted_keys: Vec<LK>,
    pub(super) changed_join_keys: OrderedSet<JK>,
    pub(super) right_rows: Vec<(RK, RV)>,
    pub(super) desired_rows: FxHashMap<OK, OV>,
    pub(super) desired_order: OrderedSet<OK>,
}

impl<LK, RK, RV, JK, OK, OV> Default for JoinScratch<LK, RK, RV, JK, OK, OV> {
    fn default() -> Self {
        Self {
            impacted: OrderedSet::default(),
            impacted_keys: Vec::new(),
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
    pub(super) fn with_right(right: RI) -> Self {
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

pub(super) fn add_index_member<I, M>(index: &mut FxHashMap<I, Vec<M>>, index_key: I, member: M)
where
    I: Hash + Eq + CellValue,
    M: Hash + Eq + CellValue,
{
    let members = index.entry(index_key).or_default();
    if !members.contains(&member) {
        members.push(member);
    }
}

pub(super) fn remove_index_member<I, M>(index: &mut FxHashMap<I, Vec<M>>, index_key: &I, member: &M)
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

pub(super) fn upsert_left<LK, LV, RK, RV, JK, OK, OV, FL>(
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

pub(super) fn remove_left<LK, LV, RK, RV, JK, OK, OV>(
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

pub(super) fn apply_left_diff<LK, LV, RK, RV, JK, OK, OV, FL>(
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
    FR: RightJoinKey<RK, RV, JK>,
{
    let join_key = right_join_key.right_join_key(&right_key, &right_value);
    state.right.write(|right| {
        let previous_join_key = right.row_join_keys.remove(&right_key);
        if previous_join_key != join_key
            && let Some(previous_join_key) = &previous_join_key
        {
            remove_index_member(&mut right.join_to_rows, previous_join_key, &right_key);
            remove_grouped_row(right, previous_join_key, &right_key);
            changed_join_keys.insert(previous_join_key.clone());
        }

        if let Some(join_key) = join_key {
            if previous_join_key.as_ref() != Some(&join_key) {
                add_index_member(&mut right.join_to_rows, join_key.clone(), right_key.clone());
            }
            upsert_grouped_row(
                right,
                join_key.clone(),
                right_key.clone(),
                right_value.clone(),
            );
            changed_join_keys.insert(join_key.clone());
            right.row_join_keys.insert(right_key.clone(), join_key);
            right.rows.insert(right_key, right_value);
        } else {
            right.rows.remove(&right_key);
        }
    });
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
            remove_grouped_row(right, &previous_join_key, right_key);
            changed_join_keys.insert(previous_join_key);
        }
        right.rows.remove(right_key);
    });
}

pub(super) fn apply_right_diff<LK, LV, RK, RV, JK, OK, OV, FR>(
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
    FR: RightJoinKey<RK, RV, JK>,
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
        FR: RightJoinKey<RK, RV, JK>,
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
                    right.grouped_rows.clear();
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

pub(super) fn observe_shared_right_diff<LK, LV, RK, RV, JK, OK, OV, FR>(
    state: &JoinState<LK, LV, RK, RV, JK, OK, OV, impl RelationIndexStorage<RK, RV, JK>>,
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
    FR: RightJoinKey<RK, RV, JK>,
{
    fn collect<RK, RV, JK, FR>(
        diff: &MapDiff<RK, RV>,
        right_join_key: &FR,
        changed_join_keys: &mut OrderedSet<JK>,
    ) where
        RK: Hash + Eq + CellValue,
        RV: CellValue,
        JK: Hash + Eq + CellValue,
        FR: RightJoinKey<RK, RV, JK>,
    {
        match diff {
            MapDiff::Initial { entries } => {
                for (key, value) in entries {
                    if let Some(join_key) = right_join_key.right_join_key(key, value) {
                        changed_join_keys.insert(join_key);
                    }
                }
            }
            MapDiff::Insert { key, value } => {
                if let Some(join_key) = right_join_key.right_join_key(key, value) {
                    changed_join_keys.insert(join_key);
                }
            }
            MapDiff::Remove { key, old_value } => {
                if let Some(join_key) = right_join_key.right_join_key(key, old_value) {
                    changed_join_keys.insert(join_key);
                }
            }
            MapDiff::Update {
                key,
                old_value,
                new_value,
            } => {
                if let Some(join_key) = right_join_key.right_join_key(key, old_value) {
                    changed_join_keys.insert(join_key);
                }
                if let Some(join_key) = right_join_key.right_join_key(key, new_value) {
                    changed_join_keys.insert(join_key);
                }
            }
            MapDiff::Batch { changes } => {
                for change in changes {
                    collect(change, right_join_key, changed_join_keys);
                }
            }
        }
    }

    collect(diff, right_join_key, changed_join_keys);
    for join_key in changed_join_keys.drain() {
        if let Some(left_keys) = state.join_to_left.get(&join_key) {
            impacted.extend(left_keys.iter().cloned());
        }
    }
}

pub(super) fn recompute_impacted<LK, LV, RK, RV, JK, OK, OV, FO>(
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
    // Pin a shared physical relationship index once for the whole root event.
    // The owned index keeps this as a direct borrow with no locking.
    let right = state.right.acquire_read();

    for left_key in scratch.impacted.drain() {
        let mut current_output_keys = state.left_output_keys.remove(&left_key).unwrap_or_default();

        scratch.desired_rows.clear();
        scratch.desired_order.clear();
        if let Some(left_value) = state.left_rows.get(&left_key) {
            scratch.right_rows.clear();
            if let Some(join_key) = state.left_join_keys.get(&left_key)
                && let Some(right_keys) = right.join_to_rows.get(join_key)
            {
                scratch
                    .right_rows
                    .extend(right_keys.iter().filter_map(|right_key| {
                        right
                            .rows
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

pub(super) fn commit_keyed_value<LK, OV>(
    output_cache: &mut FxHashMap<LK, OV>,
    left_key: LK,
    desired_value: Option<OV>,
    changes: &mut Vec<MapDiff<LK, OV>>,
) where
    LK: Hash + Eq + CellValue,
    OV: CellValue,
{
    match (output_cache.entry(left_key.clone()), desired_value) {
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
    right: &RelationIndex<RK, RV, JK>,
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
            .map(|left_key| {
                let desired = state.left_rows.get(left_key).and_then(|left_value| {
                    let right_rows = state
                        .left_join_keys
                        .get(left_key)
                        .and_then(|join_key| right.grouped_rows.get(join_key))
                        .map_or(&[][..], Vec::as_slice);
                    compute_value(left_key, left_value, right_rows)
                });
                (left_key.clone(), desired)
            })
            .collect()
    })
}

pub(super) fn recompute_keyed_impacted<LK, LV, RK, RV, JK, OV, FO>(
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
    scratch.impacted.move_into(&mut scratch.impacted_keys);
    let impacted = &scratch.impacted_keys;
    // Keep one shared-index guard across estimation and all row lookups for
    // this callback. In particular, Rayon workers share this pinned read.
    let right = state.right.acquire_read();
    let estimated_work = impacted.iter().fold(0_usize, |work, left_key| {
        let fanout = state
            .left_join_keys
            .get(left_key)
            .and_then(|join_key| right.join_to_rows.get(join_key))
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
        let desired = compute_keyed_parallel(pool, state, &right, impacted, compute_value);
        drop(right);
        for (left_key, desired_value) in desired {
            commit_keyed_value(
                &mut state.output_cache,
                left_key,
                desired_value,
                &mut changes,
            );
        }
        scratch.impacted_keys.clear();
        return changes;
    }

    for left_key in impacted {
        let desired_value = state.left_rows.get(left_key).and_then(|left_value| {
            let right_rows = state
                .left_join_keys
                .get(left_key)
                .and_then(|join_key| right.grouped_rows.get(join_key))
                .map_or(&[][..], Vec::as_slice);
            compute_value(left_key, left_value, right_rows)
        });
        commit_keyed_value(
            &mut state.output_cache,
            left_key.clone(),
            desired_value,
            &mut changes,
        );
    }
    scratch.impacted_keys.clear();

    changes
}
