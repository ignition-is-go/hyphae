use std::{collections::hash_map::Entry, hash::Hash, marker::PhantomData, sync::Arc};

use rustc_hash::FxHashMap;

use crate::{
    cell_map::MapDiff,
    traits::{CellValue, RightJoinKey},
};

use super::{
    super::{
        join_runtime::{RelationIndex, RelationIndexStorage},
        ordered_set::OrderedSet,
    },
    declaration::{JCons, JNil, StageProject},
};

/// Executable state for one stage of an arbitrary-length join region.
///
/// This is deliberately independent of query installation. Each instantiated
/// stage owns its typed relationship index; a later region executor can chain
/// as many differently typed states as its stage list requires.
pub(super) struct StageRuntimeState<
    K,
    I,
    RK,
    RV,
    JK,
    O,
    LeftKey,
    RightKeyFn,
    Project,
    RI = RelationIndex<RK, RV, JK>,
> where
    K: Hash + Eq + CellValue,
    I: CellValue,
    RK: Hash + Eq + CellValue,
    RV: CellValue,
    JK: Hash + Eq + CellValue,
    O: CellValue,
{
    left_rows: FxHashMap<K, I>,
    left_join_keys: FxHashMap<K, JK>,
    join_to_left: FxHashMap<JK, Vec<K>>,
    right: RI,
    output_cache: FxHashMap<K, O>,
    left_key: Arc<LeftKey>,
    right_key: Arc<RightKeyFn>,
    project: Arc<Project>,
    _right_types: PhantomData<fn() -> (RK, RV)>,
}

fn add_index_member<I, M>(index: &mut FxHashMap<I, Vec<M>>, index_key: I, member: M)
where
    I: Hash + Eq,
    M: Eq,
{
    let members = index.entry(index_key).or_default();
    if !members.contains(&member) {
        members.push(member);
    }
}

fn remove_index_member<I, M>(index: &mut FxHashMap<I, Vec<M>>, index_key: &I, member: &M)
where
    I: Hash + Eq,
    M: Eq,
{
    if let Some(members) = index.get_mut(index_key) {
        members.retain(|candidate| candidate != member);
        if members.is_empty() {
            index.remove(index_key);
        }
    }
}

fn upsert_relation<RK, RV, JK, FR>(
    index: &mut RelationIndex<RK, RV, JK>,
    right_key: &FR,
    key: RK,
    value: RV,
    changed: &mut OrderedSet<JK>,
) where
    RK: Hash + Eq + CellValue,
    RV: CellValue,
    JK: Hash + Eq + CellValue,
    FR: RightJoinKey<RK, RV, JK>,
{
    let new_join_key = right_key.right_join_key(&key, &value);
    let old_join_key = index.row_join_keys.remove(&key);
    if old_join_key != new_join_key
        && let Some(old) = &old_join_key
    {
        remove_index_member(&mut index.join_to_rows, old, &key);
        changed.insert(old.clone());
    }
    if let Some(join_key) = new_join_key {
        if old_join_key.as_ref() != Some(&join_key) {
            add_index_member(&mut index.join_to_rows, join_key.clone(), key.clone());
        }
        changed.insert(join_key.clone());
        index.row_join_keys.insert(key.clone(), join_key);
        index.rows.insert(key, value);
    } else {
        index.rows.remove(&key);
    }
}

fn remove_relation<RK, RV, JK>(
    index: &mut RelationIndex<RK, RV, JK>,
    key: &RK,
    changed: &mut OrderedSet<JK>,
) where
    RK: Hash + Eq + CellValue,
    RV: CellValue,
    JK: Hash + Eq + CellValue,
{
    if let Some(join_key) = index.row_join_keys.remove(key) {
        remove_index_member(&mut index.join_to_rows, &join_key, key);
        changed.insert(join_key);
    }
    index.rows.remove(key);
}

impl<K, I, RK, RV, JK, O, LeftKey, RightKeyFn, Project>
    StageRuntimeState<K, I, RK, RV, JK, O, LeftKey, RightKeyFn, Project>
where
    K: Hash + Eq + CellValue,
    I: CellValue,
    RK: Hash + Eq + CellValue,
    RV: CellValue,
    JK: Hash + Eq + CellValue,
    O: CellValue,
    LeftKey: Fn(&K, &I) -> JK,
    RightKeyFn: RightJoinKey<RK, RV, JK>,
    Project: StageProject<K, I, RK, RV, O>,
{
    pub(super) fn new(left_key: LeftKey, right_key: RightKeyFn, project: Project) -> Self {
        Self::with_index(left_key, right_key, project, RelationIndex::default())
    }

    pub(super) fn apply_right_diff(&mut self, diff: &MapDiff<RK, RV>) -> Vec<MapDiff<K, O>> {
        self.apply_right_diff_policy(diff, true)
    }
}

impl<K, I, RK, RV, JK, O, LeftKey, RightKeyFn, Project, RI>
    StageRuntimeState<K, I, RK, RV, JK, O, LeftKey, RightKeyFn, Project, RI>
where
    K: Hash + Eq + CellValue,
    I: CellValue,
    RK: Hash + Eq + CellValue,
    RV: CellValue,
    JK: Hash + Eq + CellValue,
    O: CellValue,
    LeftKey: Fn(&K, &I) -> JK,
    RightKeyFn: RightJoinKey<RK, RV, JK>,
    Project: StageProject<K, I, RK, RV, O>,
    RI: RelationIndexStorage<RK, RV, JK>,
{
    pub(super) fn with_index(
        left_key: LeftKey,
        right_key: RightKeyFn,
        project: Project,
        right: RI,
    ) -> Self {
        Self {
            left_rows: FxHashMap::default(),
            left_join_keys: FxHashMap::default(),
            join_to_left: FxHashMap::default(),
            right,
            output_cache: FxHashMap::default(),
            left_key: Arc::new(left_key),
            right_key: Arc::new(right_key),
            project: Arc::new(project),
            _right_types: PhantomData,
        }
    }

    /// Apply one left event and return the resulting output changes.
    pub(super) fn apply_left_diff(&mut self, diff: &MapDiff<K, I>) -> Vec<MapDiff<K, O>> {
        if let MapDiff::Batch { changes } = diff {
            return changes
                .iter()
                .flat_map(|change| self.apply_left_diff(change))
                .collect();
        }

        let mut impacted = OrderedSet::default();
        let mut pending = vec![diff];
        while let Some(change) = pending.pop() {
            match change {
                MapDiff::Initial { entries } => {
                    impacted.extend(self.left_rows.keys().cloned());
                    self.left_rows.clear();
                    self.left_join_keys.clear();
                    self.join_to_left.clear();
                    for (key, value) in entries {
                        self.upsert_left(key.clone(), value.clone(), &mut impacted);
                    }
                }
                MapDiff::Insert { key, value }
                | MapDiff::Update {
                    key,
                    new_value: value,
                    ..
                } => self.upsert_left(key.clone(), value.clone(), &mut impacted),
                MapDiff::Remove { key, .. } => self.remove_left(key, &mut impacted),
                MapDiff::Batch { changes } => pending.extend(changes.iter().rev()),
            }
        }
        self.recompute_impacted(&mut impacted)
    }

    fn apply_left_batch(&mut self, changes: &[MapDiff<K, I>]) -> Vec<MapDiff<K, O>> {
        let mut impacted = OrderedSet::default();
        let mut pending: Vec<_> = changes.iter().rev().collect();
        while let Some(change) = pending.pop() {
            match change {
                MapDiff::Initial { entries } => {
                    impacted.extend(self.left_rows.keys().cloned());
                    self.left_rows.clear();
                    self.left_join_keys.clear();
                    self.join_to_left.clear();
                    for (key, value) in entries {
                        self.upsert_left(key.clone(), value.clone(), &mut impacted);
                    }
                }
                MapDiff::Insert { key, value }
                | MapDiff::Update {
                    key,
                    new_value: value,
                    ..
                } => self.upsert_left(key.clone(), value.clone(), &mut impacted),
                MapDiff::Remove { key, .. } => self.remove_left(key, &mut impacted),
                MapDiff::Batch { changes } => pending.extend(changes.iter().rev()),
            }
        }
        self.recompute_impacted(&mut impacted)
    }

    /// Apply one right event and return changes for every affected left row.
    pub(super) fn apply_right_diff_policy(
        &mut self,
        diff: &MapDiff<RK, RV>,
        maintain: bool,
    ) -> Vec<MapDiff<K, O>> {
        if let MapDiff::Batch { changes } = diff {
            return changes
                .iter()
                .flat_map(|change| self.apply_right_diff_policy(change, maintain))
                .collect();
        }
        self.apply_right_batch(std::slice::from_ref(diff), maintain)
    }

    fn apply_right_batch(
        &mut self,
        changes: &[MapDiff<RK, RV>],
        maintain: bool,
    ) -> Vec<MapDiff<K, O>> {
        let mut changed_join_keys = OrderedSet::default();
        if maintain {
            let right_key = self.right_key.as_ref();
            self.right.write(|index| {
                let mut pending: Vec<_> = changes.iter().rev().collect();
                while let Some(change) = pending.pop() {
                    match change {
                        MapDiff::Initial { entries } => {
                            changed_join_keys.extend(index.row_join_keys.values().cloned());
                            index.rows.clear();
                            index.row_join_keys.clear();
                            index.join_to_rows.clear();
                            for (key, value) in entries {
                                upsert_relation(
                                    index,
                                    right_key,
                                    key.clone(),
                                    value.clone(),
                                    &mut changed_join_keys,
                                );
                            }
                        }
                        MapDiff::Insert { key, value }
                        | MapDiff::Update {
                            key,
                            new_value: value,
                            ..
                        } => {
                            upsert_relation(
                                index,
                                right_key,
                                key.clone(),
                                value.clone(),
                                &mut changed_join_keys,
                            );
                        }
                        MapDiff::Remove { key, .. } => {
                            remove_relation(index, key, &mut changed_join_keys);
                        }
                        MapDiff::Batch { changes } => pending.extend(changes.iter().rev()),
                    }
                }
            });
        } else {
            let mut pending: Vec<_> = changes.iter().rev().collect();
            while let Some(change) = pending.pop() {
                match change {
                    MapDiff::Initial { entries } => {
                        // Observers run after the one maintaining shard has
                        // replaced the shared physical index. Include their
                        // complete local dependency set as well as new keys so
                        // buckets removed by Initial are invalidated.
                        changed_join_keys.extend(self.join_to_left.keys().cloned());
                        changed_join_keys.extend(
                            entries.iter().filter_map(|(key, value)| {
                                self.right_key.right_join_key(key, value)
                            }),
                        );
                    }
                    MapDiff::Insert { key, value } => {
                        if let Some(join_key) = self.right_key.right_join_key(key, value) {
                            changed_join_keys.insert(join_key);
                        }
                    }
                    MapDiff::Update {
                        key,
                        old_value,
                        new_value,
                    } => {
                        if let Some(join_key) = self.right_key.right_join_key(key, old_value) {
                            changed_join_keys.insert(join_key);
                        }
                        if let Some(join_key) = self.right_key.right_join_key(key, new_value) {
                            changed_join_keys.insert(join_key);
                        }
                    }
                    MapDiff::Remove { key, old_value } => {
                        if let Some(join_key) = self.right_key.right_join_key(key, old_value) {
                            changed_join_keys.insert(join_key);
                        }
                    }
                    MapDiff::Batch { changes } => pending.extend(changes.iter().rev()),
                }
            }
        }
        let mut impacted = OrderedSet::default();
        for join_key in changed_join_keys.drain() {
            if let Some(left_keys) = self.join_to_left.get(&join_key) {
                impacted.extend(left_keys.iter().cloned());
            }
        }
        self.recompute_impacted(&mut impacted)
    }

    fn upsert_left(&mut self, key: K, value: I, impacted: &mut OrderedSet<K>) {
        let join_key = (self.left_key.as_ref())(&key, &value);
        match self.left_join_keys.insert(key.clone(), join_key.clone()) {
            Some(old_join_key) if old_join_key != join_key => {
                remove_index_member(&mut self.join_to_left, &old_join_key, &key);
                add_index_member(&mut self.join_to_left, join_key, key.clone());
            }
            Some(_) => {}
            None => add_index_member(&mut self.join_to_left, join_key, key.clone()),
        }
        self.left_rows.insert(key.clone(), value);
        impacted.insert(key);
    }

    fn remove_left(&mut self, key: &K, impacted: &mut OrderedSet<K>) {
        if let Some(join_key) = self.left_join_keys.remove(key) {
            remove_index_member(&mut self.join_to_left, &join_key, key);
        }
        if self.left_rows.remove(key).is_some() || self.output_cache.contains_key(key) {
            impacted.insert(key.clone());
        }
    }

    fn recompute_impacted(&mut self, impacted: &mut OrderedSet<K>) -> Vec<MapDiff<K, O>> {
        let mut changes = Vec::new();
        let mut right_matches = Vec::new();
        let right = self.right.acquire_read();
        for key in impacted.drain() {
            let desired = self.left_rows.get(&key).and_then(|input| {
                right_matches.clear();
                if let Some(join_key) = self.left_join_keys.get(&key)
                    && let Some(right_keys) = right.join_to_rows.get(join_key)
                {
                    right_matches.extend(right_keys.iter().filter_map(|right_key| {
                        right
                            .rows
                            .get(right_key)
                            .map(|value| (right_key.clone(), value.clone()))
                    }));
                }
                self.project.project(&key, input, &right_matches)
            });

            match (self.output_cache.entry(key.clone()), desired) {
                (Entry::Occupied(mut entry), Some(new_value)) if entry.get() != &new_value => {
                    let old_value = entry.insert(new_value.clone());
                    changes.push(MapDiff::Update {
                        key,
                        old_value,
                        new_value,
                    });
                }
                (Entry::Occupied(entry), None) => {
                    let (key, old_value) = entry.remove_entry();
                    changes.push(MapDiff::Remove { key, old_value });
                }
                (Entry::Vacant(entry), Some(value)) => {
                    entry.insert(value.clone());
                    changes.push(MapDiff::Insert { key, value });
                }
                (Entry::Occupied(_), Some(_)) | (Entry::Vacant(_), None) => {}
            }
        }
        changes
    }
}

pub(super) fn batch_has_unique_atomic_keys<K, V>(changes: &[MapDiff<K, V>]) -> bool
where
    K: Hash + Eq + Clone,
{
    fn visit<K: Hash + Eq + Clone, V>(
        diff: &MapDiff<K, V>,
        seen: &mut rustc_hash::FxHashSet<K>,
    ) -> bool {
        match diff {
            MapDiff::Insert { key, .. }
            | MapDiff::Update { key, .. }
            | MapDiff::Remove { key, .. } => seen.insert(key.clone()),
            MapDiff::Batch { changes } => changes.iter().all(|change| visit(change, seen)),
            MapDiff::Initial { .. } => false,
        }
    }

    let mut seen = rustc_hash::FxHashSet::default();
    changes.iter().all(|change| visit(change, &mut seen))
}

/// The statically dispatched execution contract for one join stage.
///
/// All row types are associated types so a heterogeneous stage list can thread
/// diffs without type erasure, allocation, or dynamic dispatch.
pub(super) trait ExecutableStage {
    type Key: Hash + Eq + CellValue;
    type Input: CellValue;
    type Output: CellValue;
    type RightKey: Hash + Eq + CellValue;
    type RightValue: CellValue;

    fn apply_left_diff(
        &mut self,
        diff: &MapDiff<Self::Key, Self::Input>,
    ) -> Vec<MapDiff<Self::Key, Self::Output>>;

    fn apply_right_diff(
        &mut self,
        diff: &MapDiff<Self::RightKey, Self::RightValue>,
        maintain: bool,
    ) -> Vec<MapDiff<Self::Key, Self::Output>>;
}

impl<K, I, RK, RV, JK, O, LeftKey, RightKeyFn, Project, RI> ExecutableStage
    for StageRuntimeState<K, I, RK, RV, JK, O, LeftKey, RightKeyFn, Project, RI>
where
    K: Hash + Eq + CellValue,
    I: CellValue,
    RK: Hash + Eq + CellValue,
    RV: CellValue,
    JK: Hash + Eq + CellValue,
    O: CellValue,
    LeftKey: Fn(&K, &I) -> JK,
    RightKeyFn: RightJoinKey<RK, RV, JK>,
    Project: StageProject<K, I, RK, RV, O>,
    RI: RelationIndexStorage<RK, RV, JK>,
{
    type Key = K;
    type Input = I;
    type Output = O;
    type RightKey = RK;
    type RightValue = RV;

    #[allow(clippy::use_self)] // Explicitly select the inherent per-member kernel.
    fn apply_left_diff(&mut self, diff: &MapDiff<K, I>) -> Vec<MapDiff<K, O>> {
        // The bulk kernel is semantics-preserving only when every flattened
        // member is atomic and owns a distinct key. Repeated keys and Initial
        // are observable state transitions and must be recomputed member by
        // member (Insert -> Update -> Remove is three logical events).
        if let MapDiff::Batch { changes } = diff
            && batch_has_unique_atomic_keys(changes)
        {
            self.apply_left_batch(changes)
        } else {
            StageRuntimeState::apply_left_diff(self, diff)
        }
    }

    fn apply_right_diff(&mut self, diff: &MapDiff<RK, RV>, maintain: bool) -> Vec<MapDiff<K, O>> {
        self.apply_right_diff_policy(diff, maintain)
    }
}

/// A statically executable heterogeneous list of stage runtime states.
pub(super) trait RuntimeStages<K, Input>
where
    K: Hash + Eq + CellValue,
    Input: CellValue,
{
    type Output: CellValue;

    fn apply_left_diff(&mut self, diff: &MapDiff<K, Input>) -> Vec<MapDiff<K, Self::Output>>;
}

impl<K, Input> RuntimeStages<K, Input> for JNil
where
    K: Hash + Eq + CellValue,
    Input: CellValue,
{
    type Output = Input;

    fn apply_left_diff(&mut self, diff: &MapDiff<K, Input>) -> Vec<MapDiff<K, Input>> {
        vec![diff.clone()]
    }
}

impl<K, Input, Head, Tail> RuntimeStages<K, Input> for JCons<Head, Tail>
where
    K: Hash + Eq + CellValue,
    Input: CellValue,
    Head: ExecutableStage<Key = K, Input = Input>,
    Tail: RuntimeStages<K, Head::Output>,
{
    type Output = Tail::Output;

    fn apply_left_diff(&mut self, diff: &MapDiff<K, Input>) -> Vec<MapDiff<K, Self::Output>> {
        let preserve_batch = matches!(diff, MapDiff::Batch { .. });
        let head_changes = self.head.apply_left_diff(diff);
        let mut output = Vec::new();
        for change in &head_changes {
            output.extend(self.tail.apply_left_diff(change));
        }
        if preserve_batch {
            vec![MapDiff::Batch { changes: output }]
        } else {
            output
        }
    }
}

/// Statically estimated cost of executing one input member through a typed
/// stage spine. The constants keep the routing decision monomorphized.
pub(super) trait RuntimeStageCost {
    const COST_UNITS: usize;
}

impl RuntimeStageCost for JNil {
    const COST_UNITS: usize = 1;
}

impl<Head, Tail> RuntimeStageCost for JCons<Head, Tail>
where
    Tail: RuntimeStageCost,
{
    // Frozen four-join measurements: routing/cloning is about six units and a
    // stage index lookup/projection/cache commit is about 24 units.
    const COST_UNITS: usize = 24_usize.saturating_add(Tail::COST_UNITS);
}

/// Construct an empty runtime with the same immutable stage configuration and
/// physical relationship indexes. This recursive contract keeps the complete
/// heterogeneous spine statically typed on stable Rust.
pub(super) trait EmptyShardRuntime {
    fn empty_shard(&self) -> Self;
}

impl EmptyShardRuntime for JNil {
    fn empty_shard(&self) -> Self {
        Self
    }
}

impl<K, I, RK, RV, JK, O, LeftKey, RightKeyFn, Project, RI, Tail> EmptyShardRuntime
    for JCons<StageRuntimeState<K, I, RK, RV, JK, O, LeftKey, RightKeyFn, Project, RI>, Tail>
where
    K: Hash + Eq + CellValue,
    I: CellValue,
    RK: Hash + Eq + CellValue,
    RV: CellValue,
    JK: Hash + Eq + CellValue,
    O: CellValue,
    RI: RelationIndexStorage<RK, RV, JK> + Clone,
    Tail: EmptyShardRuntime,
{
    fn empty_shard(&self) -> Self {
        Self {
            head: StageRuntimeState {
                left_rows: FxHashMap::default(),
                left_join_keys: FxHashMap::default(),
                join_to_left: FxHashMap::default(),
                right: self.head.right.clone(),
                output_cache: FxHashMap::default(),
                left_key: Arc::clone(&self.head.left_key),
                right_key: Arc::clone(&self.head.right_key),
                project: Arc::clone(&self.head.project),
                _right_types: PhantomData,
            },
            tail: self.tail.empty_shard(),
        }
    }
}

/// Snapshot access is deliberately anchored at the typed head input rather
/// than using type erasure or stage-number dispatch.
pub(super) trait HeadInputSnapshot<K, Input> {
    fn head_input(&self, key: &K) -> Option<Input>;
}

impl<K, Input, Head, Tail> HeadInputSnapshot<K, Input> for JCons<Head, Tail>
where
    K: Hash + Eq + CellValue,
    Input: CellValue,
    // All executable heads in this module are StageRuntimeState; the method is
    // supplied below by the private typed accessor trait.
    Head: ExecutableStage<Key = K, Input = Input> + HeadRows<K, Input>,
{
    fn head_input(&self, key: &K) -> Option<Input> {
        self.head.head_row(key)
    }
}

pub(super) trait HeadRows<K, Input> {
    fn head_row(&self, key: &K) -> Option<Input>;
}

impl<K, I, RK, RV, JK, O, LeftKey, RightKeyFn, Project, RI> HeadRows<K, I>
    for StageRuntimeState<K, I, RK, RV, JK, O, LeftKey, RightKeyFn, Project, RI>
where
    K: Hash + Eq + CellValue,
    I: CellValue,
    RK: Hash + Eq + CellValue,
    RV: CellValue,
    JK: Hash + Eq + CellValue,
    O: CellValue,
{
    fn head_row(&self, key: &K) -> Option<I> {
        self.left_rows.get(key).cloned()
    }
}
