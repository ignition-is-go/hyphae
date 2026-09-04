use std::{
    any::TypeId,
    collections::hash_map::Entry,
    hash::{Hash, Hasher},
    sync::{Arc, Mutex},
};

use rustc_hash::{FxHashMap, FxHasher};

use crate::{
    cell_map::MapDiff,
    subscription::SubscriptionGuard,
    traits::{CellValue, RightJoinKey},
};

use super::{
    join_lifecycle::{
        BatchedChanges, InstallRegionRights, LegacyTransaction, RegionHost, RootRegistrationOrder,
        RuntimeStorage, install_region_runtime,
    },
    ordered_set::OrderedSet,
};

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
pub(super) struct RelationIndex<RK, RV, JK> {
    pub(super) rows: FxHashMap<RK, RV>,
    pub(super) row_join_keys: FxHashMap<RK, JK>,
    pub(super) join_to_rows: FxHashMap<JK, Vec<RK>>,
    pub(super) grouped_rows: FxHashMap<JK, Vec<(RK, RV)>>,
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

pub(super) trait RelationIndexStorage<RK, RV, JK>: Send + Sync + 'static {
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

struct JoinScratch<LK, RK, RV, JK, OK, OV> {
    impacted: OrderedSet<LK>,
    impacted_keys: Vec<LK>,
    changed_join_keys: OrderedSet<JK>,
    right_rows: Vec<(RK, RV)>,
    desired_rows: FxHashMap<OK, OV>,
    desired_order: OrderedSet<OK>,
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

fn observe_shared_right_diff<LK, LV, RK, RV, JK, OK, OV, FR>(
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

fn commit_keyed_value<LK, OV>(
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

// ── The public entry point ──────────────────────────────────────────────

/// Emit a batch of output diffs through `sink`.
///
/// Preserves the original `apply_batch` semantics observed by downstream
/// subscribers: every non-empty group of output diffs produced from a single
/// upstream diff is delivered as one `MapDiff::Batch`, even when the group
/// contains a single change. Empty batches are dropped.
fn emit_changes<OK, OV>(
    sink: &crate::map_query::BoxedMapDiffSink<OK, OV>,
    changes: Vec<MapDiff<OK, OV>>,
) where
    OK: Hash + Eq + CellValue,
    OV: CellValue,
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
pub fn install_join_runtime_via_query<LK, LV, RK, RV, JK, OK, OV, L, R, FL, FR, FO>(
    cx: &mut crate::map_query::compiler::CompileContext,
    left: L,
    right: R,
    left_join_key: FL,
    right_join_key: FR,
    compute_rows: FO,
    sink: crate::map_query::BoxedMapDiffSink<OK, OV>,
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
    FR: RightJoinKey<RK, RV, JK> + Send + Sync + 'static,
    FO: Fn(&LK, &LV, &[(RK, RV)]) -> Vec<(OK, OV)> + Send + Sync + 'static,
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

    let mut guards = crate::map_query::compile_runtime_into(left, cx, Arc::new(left_sink));
    guards.extend(crate::map_query::compile_runtime_into(
        right,
        cx,
        Arc::new(right_sink),
    ));
    guards
}

#[allow(clippy::missing_const_for_fn)]
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

    #[allow(clippy::too_many_lines)]
    fn route_right<FR>(
        &mut self,
        diff: &MapDiff<RK, RV>,
        right_join_key: &FR,
    ) -> Vec<Vec<MapDiff<RK, RV>>>
    where
        FR: RightJoinKey<RK, RV, JK>,
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
                        if let Some(join_key) = right_join_key.right_join_key(&key, &value) {
                            let route = shard_for(&join_key, self.shards.len());
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
                }
                MapDiff::Insert { key, value } => {
                    if let Some(join_key) = right_join_key.right_join_key(&key, &value) {
                        let route = shard_for(&join_key, self.shards.len());
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
                MapDiff::Update {
                    key,
                    old_value,
                    new_value,
                } => {
                    let old_route = self.right_routes.remove(&key).or_else(|| {
                        right_join_key
                            .right_join_key(&key, &old_value)
                            .map(|join_key| shard_for(&join_key, self.shards.len()))
                    });
                    let new_route = right_join_key
                        .right_join_key(&key, &new_value)
                        .map(|join_key| shard_for(&join_key, self.shards.len()));
                    match (old_route, new_route) {
                        (Some(old_route), Some(new_route)) if old_route == new_route => {
                            push_routed(
                                &mut routed,
                                new_route,
                                MapDiff::Update {
                                    key: key.clone(),
                                    old_value,
                                    new_value,
                                },
                            );
                            self.right_routes.insert(key, new_route);
                        }
                        (Some(old_route), Some(new_route)) => {
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
                            self.right_routes.insert(key, new_route);
                        }
                        (Some(old_route), None) => {
                            push_routed(&mut routed, old_route, MapDiff::Remove { key, old_value });
                        }
                        (None, Some(new_route)) => {
                            push_routed(
                                &mut routed,
                                new_route,
                                MapDiff::Insert {
                                    key: key.clone(),
                                    value: new_value,
                                },
                            );
                            self.right_routes.insert(key, new_route);
                        }
                        (None, None) => {}
                    }
                }
                MapDiff::Remove { key, old_value } => {
                    let route = self.right_routes.remove(&key).or_else(|| {
                        right_join_key
                            .right_join_key(&key, &old_value)
                            .map(|join_key| shard_for(&join_key, self.shards.len()))
                    });
                    if let Some(route) = route {
                        push_routed(&mut routed, route, MapDiff::Remove { key, old_value });
                    }
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
                let ordinal = change
                    .atomic_key()
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
    FR: RightJoinKey<RK, RV, JK> + Sync,
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
    let right = state.right.acquire_read();
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
}

/// Install the zero-or-one-output, left-key-preserving join runtime.
pub fn install_keyed_join_runtime_via_query<LK, LV, RK, RV, JK, OV, L, R, FL, FR, FO>(
    cx: &mut crate::map_query::compiler::CompileContext,
    left: L,
    right: R,
    left_join_key: FL,
    right_join_key: FR,
    compute_value: FO,
    sink: crate::map_query::BoxedMapDiffSink<LK, OV>,
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
    FR: RightJoinKey<RK, RV, JK> + Send + Sync + 'static,
    FO: Fn(&LK, &LV, &[(RK, RV)]) -> Option<OV> + Send + Sync + 'static,
{
    let relation = cx.take_relation_hint();
    // A relation marker alone is not enough to prove that two right inputs
    // have the same semantics. Only raw source boundaries have a stable
    // identity suitable for index reuse; projections, filters, and all other
    // operator nodes keep an independent index even when they share a root.
    if let Some(relation) = relation.filter(|_| right.raw_source_identity().is_some()) {
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
fn install_keyed_join_runtime_with_index<LK, LV, RK, RV, JK, OV, L, R, FL, FR, FO, RI>(
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
    sink: crate::map_query::BoxedMapDiffSink<LK, OV>,
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
    FR: RightJoinKey<RK, RV, JK> + Send + Sync + 'static,
    FO: Fn(&LK, &LV, &[(RK, RV)]) -> Option<OV> + Send + Sync + 'static,
    RI: RelationIndexStorage<RK, RV, JK>,
{
    let shared_index = relationship_binding
        .as_ref()
        .map(|(_, index)| index.clone());
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
        let shared_index = shared_index;
        move |diff: &MapDiff<RK, RV>| {
            let mut state = state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut scratch = std::mem::take(&mut state.scratch);
            if shared_index
                .as_ref()
                .is_none_or(crate::map_query::compiler::DeferredPhysical::maintains_index)
            {
                apply_right_diff(
                    &mut state,
                    diff,
                    right_join_key.as_ref(),
                    &mut scratch.impacted,
                    &mut scratch.changed_join_keys,
                );
            } else {
                observe_shared_right_diff(
                    &state,
                    diff,
                    right_join_key.as_ref(),
                    &mut scratch.impacted,
                    &mut scratch.changed_join_keys,
                );
            }
            let changes =
                recompute_keyed_impacted(&mut state, &mut scratch, compute_value.as_ref());
            state.scratch = scratch;
            emit_changes(sink.as_ref(), changes);
            drop(state);
        }
    };

    let mut guards = crate::map_query::compile_runtime_into(left, cx, Arc::new(left_sink));
    if let Some((relation, index)) = relationship_binding {
        guards.extend(cx.with_root_relation_index(relation, index, |cx| {
            crate::map_query::compile_runtime_into(right, cx, Arc::new(right_sink))
        }));
    } else {
        guards.extend(crate::map_query::compile_runtime_into(
            right,
            cx,
            Arc::new(right_sink),
        ));
    }
    guards
}

type TwoFirstState<LK, LV, RK, RV, JK, MV> = JoinState<LK, LV, RK, RV, JK, LK, MV>;
type TwoSecondState<LK, MV, RK, RV, JK, OV> = JoinState<LK, MV, RK, RV, JK, LK, OV>;
type TwoFirstShards<LK, LV, RK, RV, JK, MV> = ShardedKeyedJoin<LK, LV, RK, RV, JK, MV>;
type TwoSecondShards<LK, MV, RK, RV, JK, OV> = ShardedKeyedJoin<LK, MV, RK, RV, JK, OV>;

struct TwoKeyedSerial<First, Second> {
    first: First,
    second: Second,
}

struct TwoKeyedSharded<First, Second> {
    first: First,
    second: Second,
}

struct TwoStageConfig<FL1, FR1, FM1, FL2, FR2, FM2> {
    left_key1: Arc<FL1>,
    right_key1: Arc<FR1>,
    project1: Arc<FM1>,
    left_key2: Arc<FL2>,
    right_key2: Arc<FR2>,
    project2: Arc<FM2>,
}

#[allow(clippy::type_complexity)]
struct TwoStageKernel<LK, LV, RK1, RV1, JK1, MV, RK2, RV2, JK2, OV, FL1, FR1, FM1, FL2, FR2, FM2>
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
{
    storage: RuntimeStorage<
        TwoKeyedSerial<
            TwoFirstState<LK, LV, RK1, RV1, JK1, MV>,
            TwoSecondState<LK, MV, RK2, RV2, JK2, OV>,
        >,
        TwoKeyedSharded<
            TwoFirstShards<LK, LV, RK1, RV1, JK1, MV>,
            TwoSecondShards<LK, MV, RK2, RV2, JK2, OV>,
        >,
    >,
    config: TwoStageConfig<FL1, FR1, FM1, FL2, FR2, FM2>,
    shard_count: usize,
}

struct TwoStageRights<First, Second> {
    first: First,
    second: Second,
}

#[allow(clippy::too_many_lines, clippy::type_complexity)]
impl<LK, LV, RK1, RV1, JK1, MV, RK2, RV2, JK2, OV, FL1, FR1, FM1, FL2, FR2, FM2>
    TwoStageKernel<LK, LV, RK1, RV1, JK1, MV, RK2, RV2, JK2, OV, FL1, FR1, FM1, FL2, FR2, FM2>
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
    FL1: Fn(&LK, &LV) -> JK1 + Send + Sync + 'static,
    FR1: RightJoinKey<RK1, RV1, JK1> + Send + Sync + 'static,
    FM1: Fn(&LK, &LV, &[(RK1, RV1)]) -> MV + Send + Sync + 'static,
    FL2: Fn(&LK, &MV) -> JK2 + Send + Sync + 'static,
    FR2: RightJoinKey<RK2, RV2, JK2> + Send + Sync + 'static,
    FM2: Fn(&LK, &MV, &[(RK2, RV2)]) -> OV + Send + Sync + 'static,
{
    fn new(
        left_key1: FL1,
        right_key1: FR1,
        project1: FM1,
        left_key2: FL2,
        right_key2: FR2,
        project2: FM2,
    ) -> Self {
        Self {
            storage: RuntimeStorage::Serial(TwoKeyedSerial {
                first: JoinState::default(),
                second: JoinState::default(),
            }),
            config: TwoStageConfig {
                left_key1: Arc::new(left_key1),
                right_key1: Arc::new(right_key1),
                project1: Arc::new(project1),
                left_key2: Arc::new(left_key2),
                right_key2: Arc::new(right_key2),
                project2: Arc::new(project2),
            },
            shard_count: query_shard_count(),
        }
    }

    fn propagate_serial(
        serial: &mut TwoKeyedSerial<
            TwoFirstState<LK, LV, RK1, RV1, JK1, MV>,
            TwoSecondState<LK, MV, RK2, RV2, JK2, OV>,
        >,
        config: &TwoStageConfig<FL1, FR1, FM1, FL2, FR2, FM2>,
    ) -> Vec<MapDiff<LK, OV>> {
        let mut scratch1 = std::mem::take(&mut serial.first.scratch);
        scratch1.impacted.move_into(&mut scratch1.impacted_keys);
        let right = serial.first.right.acquire_read();
        let mut scratch2 = std::mem::take(&mut serial.second.scratch);

        for key in &scratch1.impacted_keys {
            let desired = serial.first.left_rows.get(key).map(|left_value| {
                let right_rows = serial
                    .first
                    .left_join_keys
                    .get(key)
                    .and_then(|join_key| right.grouped_rows.get(join_key))
                    .map_or(&[][..], Vec::as_slice);
                (config.project1)(key, left_value, right_rows)
            });

            match (serial.first.output_cache.entry(key.clone()), desired) {
                (Entry::Occupied(mut entry), Some(value)) if entry.get() != &value => {
                    entry.insert(value.clone());
                    upsert_left(
                        &mut serial.second,
                        key.clone(),
                        value,
                        config.left_key2.as_ref(),
                        &mut scratch2.impacted,
                    );
                }
                (Entry::Vacant(entry), Some(value)) => {
                    entry.insert(value.clone());
                    upsert_left(
                        &mut serial.second,
                        key.clone(),
                        value,
                        config.left_key2.as_ref(),
                        &mut scratch2.impacted,
                    );
                }
                (Entry::Occupied(entry), None) => {
                    entry.remove();
                    remove_left(&mut serial.second, key, &mut scratch2.impacted);
                }
                _ => {}
            }
        }
        scratch1.impacted_keys.clear();
        serial.first.scratch = scratch1;

        let output =
            recompute_keyed_impacted(&mut serial.second, &mut scratch2, &|key, value, rights| {
                Some((config.project2)(key, value, rights))
            });
        serial.second.scratch = scratch2;
        output
    }

    fn propagate_atomic_left(
        serial: &mut TwoKeyedSerial<
            TwoFirstState<LK, LV, RK1, RV1, JK1, MV>,
            TwoSecondState<LK, MV, RK2, RV2, JK2, OV>,
        >,
        config: &TwoStageConfig<FL1, FR1, FM1, FL2, FR2, FM2>,
        key: &LK,
        value: &LV,
    ) -> Vec<MapDiff<LK, OV>> {
        serial.first.parallel_active = false;
        serial.second.parallel_active = false;
        let join_key = (config.left_key1)(key, value);
        match serial
            .first
            .left_join_keys
            .insert(key.clone(), join_key.clone())
        {
            Some(previous) if previous != join_key => {
                remove_index_member(&mut serial.first.join_to_left, &previous, key);
                add_index_member(&mut serial.first.join_to_left, join_key, key.clone());
            }
            Some(_) => {}
            None => add_index_member(&mut serial.first.join_to_left, join_key, key.clone()),
        }
        serial.first.left_rows.insert(key.clone(), value.clone());

        let right = serial.first.right.acquire_read();
        let right_rows = serial
            .first
            .left_join_keys
            .get(key)
            .and_then(|join_key| right.grouped_rows.get(join_key))
            .map_or(&[][..], Vec::as_slice);
        let middle = (config.project1)(key, value, right_rows);
        if serial.first.output_cache.get(key) == Some(&middle) {
            return Vec::new();
        }
        serial
            .first
            .output_cache
            .insert(key.clone(), middle.clone());

        let join_key = (config.left_key2)(key, &middle);
        match serial
            .second
            .left_join_keys
            .insert(key.clone(), join_key.clone())
        {
            Some(previous) if previous != join_key => {
                remove_index_member(&mut serial.second.join_to_left, &previous, key);
                add_index_member(&mut serial.second.join_to_left, join_key, key.clone());
            }
            Some(_) => {}
            None => add_index_member(&mut serial.second.join_to_left, join_key, key.clone()),
        }
        serial.second.left_rows.insert(key.clone(), middle.clone());

        let right = serial.second.right.acquire_read();
        let right_rows = serial
            .second
            .left_join_keys
            .get(key)
            .and_then(|join_key| right.grouped_rows.get(join_key))
            .map_or(&[][..], Vec::as_slice);
        let final_value = (config.project2)(key, &middle, right_rows);
        let mut output = Vec::with_capacity(1);
        commit_keyed_value(
            &mut serial.second.output_cache,
            key.clone(),
            Some(final_value),
            &mut output,
        );
        output
    }

    fn propagate_sharded(
        sharded: &mut TwoKeyedSharded<
            TwoFirstShards<LK, LV, RK1, RV1, JK1, MV>,
            TwoSecondShards<LK, MV, RK2, RV2, JK2, OV>,
        >,
        config: &TwoStageConfig<FL1, FR1, FM1, FL2, FR2, FM2>,
        first_shard_changes: Vec<Vec<MapDiff<LK, MV>>>,
        first_event_ordinals: &FxHashMap<LK, u64>,
    ) -> Vec<MapDiff<LK, OV>> {
        let intermediate = sharded
            .first
            .merge_changes(first_shard_changes, first_event_ordinals);
        let (routed_second, second_event_ordinals) = sharded
            .second
            .route_left_owned(intermediate, config.left_key2.as_ref());
        let second_shard_changes = process_left_shards(
            &mut sharded.second,
            &routed_second,
            config.left_key2.as_ref(),
            config.project2.as_ref(),
        );
        sharded
            .second
            .merge_changes(second_shard_changes, &second_event_ordinals)
    }

    fn build_sharded(
        serial: &TwoKeyedSerial<
            TwoFirstState<LK, LV, RK1, RV1, JK1, MV>,
            TwoSecondState<LK, MV, RK2, RV2, JK2, OV>,
        >,
        config: &TwoStageConfig<FL1, FR1, FM1, FL2, FR2, FM2>,
        shard_count: usize,
    ) -> TwoKeyedSharded<
        TwoFirstShards<LK, LV, RK1, RV1, JK1, MV>,
        TwoSecondShards<LK, MV, RK2, RV2, JK2, OV>,
    > {
        let mut first = ShardedKeyedJoin::new(shard_count);
        let first_left = MapDiff::Initial {
            entries: state_left_entries(&serial.first),
        };
        let (routed, _) = first.route_left(&first_left, config.left_key1.as_ref());
        let _ = process_left_shards(
            &mut first,
            &routed,
            config.left_key1.as_ref(),
            config.project1.as_ref(),
        );
        let first_right = MapDiff::Initial {
            entries: state_right_entries(&serial.first),
        };
        let routed = first.route_right(&first_right, config.right_key1.as_ref());
        let _ = process_right_shards(
            &mut first,
            &routed,
            config.right_key1.as_ref(),
            config.project1.as_ref(),
        );

        let mut second = ShardedKeyedJoin::new(shard_count);
        let second_left = MapDiff::Initial {
            entries: state_left_entries(&serial.second),
        };
        let (routed, _) = second.route_left(&second_left, config.left_key2.as_ref());
        let _ = process_left_shards(
            &mut second,
            &routed,
            config.left_key2.as_ref(),
            config.project2.as_ref(),
        );
        let second_right = MapDiff::Initial {
            entries: state_right_entries(&serial.second),
        };
        let routed = second.route_right(&second_right, config.right_key2.as_ref());
        let _ = process_right_shards(
            &mut second,
            &routed,
            config.right_key2.as_ref(),
            config.project2.as_ref(),
        );
        TwoKeyedSharded { first, second }
    }

    fn promote_for(&mut self, work: usize) {
        if self.storage.is_serial() && self.shard_count > 1 && work >= 8_192 {
            let config = &self.config;
            let shard_count = self.shard_count;
            self.storage
                .promote_with(|serial| Self::build_sharded(serial, config, shard_count));
        }
    }

    fn apply_left_diff(&mut self, diff: &MapDiff<LK, LV>) -> BatchedChanges<LK, OV> {
        self.promote_for(diff.work_items());
        let config = &self.config;
        let changes = match &mut self.storage {
            RuntimeStorage::Serial(serial) => match diff {
                MapDiff::Insert { key, value }
                | MapDiff::Update {
                    key,
                    new_value: value,
                    ..
                } => Self::propagate_atomic_left(serial, config, key, value),
                MapDiff::Initial { .. } | MapDiff::Remove { .. } | MapDiff::Batch { .. } => {
                    let mut scratch = std::mem::take(&mut serial.first.scratch);
                    apply_left_diff(
                        &mut serial.first,
                        diff,
                        config.left_key1.as_ref(),
                        &mut scratch.impacted,
                    );
                    serial.first.scratch = scratch;
                    Self::propagate_serial(serial, config)
                }
            },
            RuntimeStorage::Sharded {
                runtime: sharded, ..
            } => {
                let (routed, event_ordinals) =
                    sharded.first.route_left(diff, config.left_key1.as_ref());
                let shard_changes = process_left_shards(
                    &mut sharded.first,
                    &routed,
                    config.left_key1.as_ref(),
                    config.project1.as_ref(),
                );
                Self::propagate_sharded(sharded, config, shard_changes, &event_ordinals)
            }
        };
        BatchedChanges(changes)
    }

    fn apply_first_right_diff(&mut self, diff: &MapDiff<RK1, RV1>) -> BatchedChanges<LK, OV> {
        self.promote_for(diff.work_items());
        let config = &self.config;
        let changes = match &mut self.storage {
            RuntimeStorage::Serial(serial) => {
                let mut scratch = std::mem::take(&mut serial.first.scratch);
                apply_right_diff(
                    &mut serial.first,
                    diff,
                    config.right_key1.as_ref(),
                    &mut scratch.impacted,
                    &mut scratch.changed_join_keys,
                );
                serial.first.scratch = scratch;
                Self::propagate_serial(serial, config)
            }
            RuntimeStorage::Sharded {
                runtime: sharded, ..
            } => {
                let routed = sharded.first.route_right(diff, config.right_key1.as_ref());
                let shard_changes = process_right_shards(
                    &mut sharded.first,
                    &routed,
                    config.right_key1.as_ref(),
                    config.project1.as_ref(),
                );
                Self::propagate_sharded(sharded, config, shard_changes, &FxHashMap::default())
            }
        };
        BatchedChanges(changes)
    }

    fn apply_second_right_diff(&mut self, diff: &MapDiff<RK2, RV2>) -> BatchedChanges<LK, OV> {
        self.promote_for(diff.work_items());
        let config = &self.config;
        let changes = match &mut self.storage {
            RuntimeStorage::Serial(serial) => {
                let mut scratch = std::mem::take(&mut serial.second.scratch);
                apply_right_diff(
                    &mut serial.second,
                    diff,
                    config.right_key2.as_ref(),
                    &mut scratch.impacted,
                    &mut scratch.changed_join_keys,
                );
                let changes = recompute_keyed_impacted(
                    &mut serial.second,
                    &mut scratch,
                    &|key, value, rights| Some((config.project2)(key, value, rights)),
                );
                serial.second.scratch = scratch;
                changes
            }
            RuntimeStorage::Sharded {
                runtime: sharded, ..
            } => {
                let routed = sharded.second.route_right(diff, config.right_key2.as_ref());
                let shard_changes = process_right_shards(
                    &mut sharded.second,
                    &routed,
                    config.right_key2.as_ref(),
                    config.project2.as_ref(),
                );
                sharded
                    .second
                    .merge_changes(shard_changes, &FxHashMap::default())
            }
        };
        BatchedChanges(changes)
    }
}

#[allow(clippy::type_complexity)]
impl<LK, LV, RK1, RV1, JK1, MV, RK2, RV2, JK2, OV, FL1, FR1, FM1, FL2, FR2, FM2, First, Second>
    InstallRegionRights<
        TwoStageKernel<LK, LV, RK1, RV1, JK1, MV, RK2, RV2, JK2, OV, FL1, FR1, FM1, FL2, FR2, FM2>,
        LK,
        OV,
        LegacyTransaction,
    > for TwoStageRights<First, Second>
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
    FL1: Fn(&LK, &LV) -> JK1 + Send + Sync + 'static,
    FR1: RightJoinKey<RK1, RV1, JK1> + Send + Sync + 'static,
    FM1: Fn(&LK, &LV, &[(RK1, RV1)]) -> MV + Send + Sync + 'static,
    FL2: Fn(&LK, &MV) -> JK2 + Send + Sync + 'static,
    FR2: RightJoinKey<RK2, RV2, JK2> + Send + Sync + 'static,
    FM2: Fn(&LK, &MV, &[(RK2, RV2)]) -> OV + Send + Sync + 'static,
    First: crate::map_query::MapQuery<Key = RK1, Value = RV1>,
    Second: crate::map_query::MapQuery<Key = RK2, Value = RV2>,
{
    fn install(
        self,
        cx: &mut crate::map_query::compiler::CompileContext,
        host: &Arc<
            RegionHost<
                TwoStageKernel<
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
                    FL1,
                    FR1,
                    FM1,
                    FL2,
                    FR2,
                    FM2,
                >,
                LK,
                OV,
                LegacyTransaction,
            >,
        >,
    ) -> Vec<SubscriptionGuard> {
        let first_host = Arc::clone(host);
        let mut guards = crate::map_query::compile_runtime_into(
            self.first,
            cx,
            Arc::new(move |diff: &MapDiff<RK1, RV1>| {
                first_host.dispatch(|kernel| kernel.apply_first_right_diff(diff));
            }),
        );

        let second_host = Arc::clone(host);
        guards.extend(crate::map_query::compile_runtime_into(
            self.second,
            cx,
            Arc::new(move |diff: &MapDiff<RK2, RV2>| {
                second_host.dispatch(|kernel| kernel.apply_second_right_diff(diff));
            }),
        ));
        guards
    }
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
    sink: crate::map_query::BoxedMapDiffSink<LK, OV>,
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
    FR1: RightJoinKey<RK1, RV1, JK1> + Send + Sync + 'static,
    FM1: Fn(&LK, &LV, &[(RK1, RV1)]) -> MV + Send + Sync + 'static,
    FL2: Fn(&LK, &MV) -> JK2 + Send + Sync + 'static,
    FR2: RightJoinKey<RK2, RV2, JK2> + Send + Sync + 'static,
    FM2: Fn(&LK, &MV, &[(RK2, RV2)]) -> OV + Send + Sync + 'static,
{
    let kernel = TwoStageKernel::new(
        left_join_key1,
        right_join_key1,
        map_first,
        left_join_key2,
        right_join_key2,
        map_second,
    );
    let right_roots = TwoStageRights {
        first: right1,
        second: right2,
    };
    install_region_runtime(
        cx,
        left,
        right_roots,
        kernel,
        RootRegistrationOrder::LeftThenRights,
        LegacyTransaction,
        sink,
        TwoStageKernel::apply_left_diff,
    )
}

#[cfg(test)]
mod read_acquisition_tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use super::*;

    struct CountingIndex {
        index: RelationIndex<usize, usize, usize>,
        reads: Arc<AtomicUsize>,
    }

    impl RelationIndexStorage<usize, usize, usize> for CountingIndex {
        type Read<'a> = &'a RelationIndex<usize, usize, usize>;

        fn acquire_read(&self) -> Self::Read<'_> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            &self.index
        }

        fn write<T>(
            &mut self,
            write: impl FnOnce(&mut RelationIndex<usize, usize, usize>) -> T,
        ) -> T {
            write(&mut self.index)
        }
    }

    #[test]
    fn keyed_callback_acquires_shared_index_once_for_all_impacted_keys() {
        let reads = Arc::new(AtomicUsize::new(0));
        let mut index = RelationIndex::default();
        index.join_to_rows.insert(0, (0..32).collect());
        for key in 0..32 {
            index.rows.insert(key, key);
            index.row_join_keys.insert(key, 0);
        }
        let mut state = JoinState::with_right(CountingIndex {
            index,
            reads: Arc::clone(&reads),
        });
        let mut scratch = JoinScratch::default();
        for key in 0..128 {
            state.left_rows.insert(key, key);
            state.left_join_keys.insert(key, 0);
            scratch.impacted.insert(key);
        }

        let changes = recompute_keyed_impacted(&mut state, &mut scratch, &|_, _, right_rows| {
            Some(right_rows.len())
        });

        assert_eq!(changes.len(), 128);
        assert_eq!(reads.load(Ordering::Relaxed), 1);
        assert!(scratch.impacted_keys.is_empty());
        assert!(scratch.impacted_keys.capacity() >= 128);
    }
}
