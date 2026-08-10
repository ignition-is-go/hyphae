#![allow(dead_code)]

//! Static type substrate for an arbitrary-length left-join region.
//!
//! This module deliberately contains no installer yet.  It describes a join
//! region without tuples (and therefore without tuple-arity limits), while
//! keeping every stage and projection statically dispatched.

use std::{collections::hash_map::Entry, hash::Hash, marker::PhantomData};

use rustc_hash::FxHashMap;

use crate::{
    cell_map::MapDiff,
    map_query::properties::{ByMapKey, Partition, PlanProperties, ZeroOrOne},
    traits::{CellValue, RightJoinKey},
};

use super::ordered_set::OrderedSet;

/// The empty join-stage list.
#[derive(Debug, Clone, Copy, Default)]
pub struct JNil;

/// A forward (execution-order) cons cell.
///
/// `Head` executes before every stage in `Tail`.  Prefer [`Push::push`] when
/// building a list: unlike prepending a conventional cons cell, it preserves
/// the order in which stages were specified.
#[derive(Debug, Clone, Copy)]
pub struct JCons<Head, Tail> {
    pub head: Head,
    pub tail: Tail,
}

/// Append one stage to a forward join-stage list.
pub trait Push<Stage> {
    type Output;

    fn push(self, stage: Stage) -> Self::Output;
}

impl<Stage> Push<Stage> for JNil {
    type Output = JCons<Stage, Self>;

    fn push(self, stage: Stage) -> Self::Output {
        JCons {
            head: stage,
            tail: self,
        }
    }
}

impl<Head, Tail, Stage> Push<Stage> for JCons<Head, Tail>
where
    Tail: Push<Stage>,
{
    type Output = JCons<Head, Tail::Output>;

    fn push(self, stage: Stage) -> Self::Output {
        JCons {
            head: self.head,
            tail: self.tail.push(stage),
        }
    }
}

/// The type-level contract of one stage in a join region.
pub trait StageSpec {
    type Key: Hash + Eq + CellValue;
    type Input: CellValue;
    type Output: CellValue;
    type InputPartition: Partition;
    type OutputPartition: Partition;
}

/// Proof that a heterogeneous stage list is a correctly threaded pipeline.
///
/// The output of each head is required to be the input of its tail.  This is
/// expressed entirely with associated-type equality and works on stable Rust.
pub trait StageList<K, Input>
where
    K: Hash + Eq + CellValue,
    Input: CellValue,
{
    type Output: CellValue;
}

impl<K, Input> StageList<K, Input> for JNil
where
    K: Hash + Eq + CellValue,
    Input: CellValue,
{
    type Output = Input;
}

impl<K, Input, Head, Tail> StageList<K, Input> for JCons<Head, Tail>
where
    K: Hash + Eq + CellValue,
    Input: CellValue,
    Head: StageSpec<Key = K, Input = Input, OutputPartition = ByMapKey<K>>,
    Tail: StageList<K, Head::Output>,
{
    type Output = Tail::Output;
}

/// A projection performed directly at a join boundary.
///
/// `None` means that the current left row is filtered out.  Consequently
/// collection, direct projection, mapping, and filtering can be composed
/// without creating an intermediate joined plan node.
pub trait StageProject<K, Input, RightKey, RightValue, Output>: Send + Sync + 'static {
    fn project(&self, key: &K, input: &Input, rights: &[(RightKey, RightValue)]) -> Option<Output>;
}

/// Adapt the ordinary `(left, Vec<right>)` projection surface.
pub struct CollectProject<F>(pub F);

impl<K, Input, RightKey, RightValue, Output, F> StageProject<K, Input, RightKey, RightValue, Output>
    for CollectProject<F>
where
    Input: Clone,
    RightValue: Clone,
    F: Fn(&K, &(Input, Vec<RightValue>)) -> Output + Send + Sync + 'static,
{
    fn project(&self, key: &K, input: &Input, rights: &[(RightKey, RightValue)]) -> Option<Output> {
        let joined = (
            input.clone(),
            rights.iter().map(|(_, value)| value.clone()).collect(),
        );
        Some((self.0)(key, &joined))
    }
}

/// Adapt a projection which consumes the indexed right matches directly.
pub struct DirectProject<F>(pub F);

impl<K, Input, RightKey, RightValue, Output, F> StageProject<K, Input, RightKey, RightValue, Output>
    for DirectProject<F>
where
    F: Fn(&K, &Input, &[(RightKey, RightValue)]) -> Output + Send + Sync + 'static,
{
    fn project(&self, key: &K, input: &Input, rights: &[(RightKey, RightValue)]) -> Option<Output> {
        Some((self.0)(key, input, rights))
    }
}

/// Map the successful output of another stage projection.
pub struct ThenMap<Project, F, Intermediate> {
    project: Project,
    map: F,
    _intermediate: PhantomData<fn() -> Intermediate>,
}

impl<Project, F, Intermediate> ThenMap<Project, F, Intermediate> {
    pub const fn new(project: Project, map: F) -> Self {
        Self {
            project,
            map,
            _intermediate: PhantomData,
        }
    }
}

impl<K, Input, RightKey, RightValue, Intermediate, Output, Project, F>
    StageProject<K, Input, RightKey, RightValue, Output> for ThenMap<Project, F, Intermediate>
where
    Intermediate: 'static,
    Project: StageProject<K, Input, RightKey, RightValue, Intermediate>,
    F: Fn(&K, &Intermediate) -> Output + Send + Sync + 'static,
{
    fn project(&self, key: &K, input: &Input, rights: &[(RightKey, RightValue)]) -> Option<Output> {
        self.project
            .project(key, input, rights)
            .map(|value| (self.map)(key, &value))
    }
}

/// Filter the successful output of another stage projection.
pub struct FilterProject<Project, Predicate> {
    project: Project,
    predicate: Predicate,
}

impl<Project, Predicate> FilterProject<Project, Predicate> {
    pub const fn new(project: Project, predicate: Predicate) -> Self {
        Self { project, predicate }
    }
}

impl<K, Input, RightKey, RightValue, Output, Project, Predicate>
    StageProject<K, Input, RightKey, RightValue, Output> for FilterProject<Project, Predicate>
where
    Output: 'static,
    Project: StageProject<K, Input, RightKey, RightValue, Output>,
    Predicate: Fn(&K, &Output) -> bool + Send + Sync + 'static,
{
    fn project(&self, key: &K, input: &Input, rights: &[(RightKey, RightValue)]) -> Option<Output> {
        self.project
            .project(key, input, rights)
            .filter(|value| (self.predicate)(key, value))
    }
}

/// One statically typed join stage.
///
/// The right plan and key extractors will be consumed by the later compiler
/// stage.  Keeping them here makes an N-stage region one concrete Rust type.
pub struct JoinStage<
    Right,
    K,
    Input,
    RightKey,
    RightValue,
    JoinKey,
    Output,
    LeftKey,
    RightKeyFn,
    Project,
> {
    pub right: Right,
    pub left_key: LeftKey,
    pub right_key: RightKeyFn,
    pub project: Project,
    _types: PhantomData<fn() -> (K, Input, RightKey, RightValue, JoinKey, Output)>,
}

impl<Right, K, Input, RightKey, RightValue, JoinKey, Output, LeftKey, RightKeyFn, Project>
    JoinStage<Right, K, Input, RightKey, RightValue, JoinKey, Output, LeftKey, RightKeyFn, Project>
{
    #[allow(clippy::too_many_arguments)]
    pub const fn new(
        right: Right,
        left_key: LeftKey,
        right_key: RightKeyFn,
        project: Project,
    ) -> Self {
        Self {
            right,
            left_key,
            right_key,
            project,
            _types: PhantomData,
        }
    }
}

impl<Right, K, Input, RightKey, RightValue, JoinKey, Output, LeftKey, RightKeyFn, Project> StageSpec
    for JoinStage<
        Right,
        K,
        Input,
        RightKey,
        RightValue,
        JoinKey,
        Output,
        LeftKey,
        RightKeyFn,
        Project,
    >
where
    K: Hash + Eq + CellValue,
    Input: CellValue,
    Output: CellValue,
    Right: Send + Sync + 'static,
    RightKey: Send + Sync + 'static,
    RightValue: Send + Sync + 'static,
    JoinKey: Send + Sync + 'static,
    LeftKey: Send + Sync + 'static,
    RightKeyFn: Send + Sync + 'static,
    Project: StageProject<K, Input, RightKey, RightValue, Output>,
{
    type Key = K;
    type Input = Input;
    type Output = Output;
    type InputPartition = ByMapKey<K>;
    type OutputPartition = ByMapKey<K>;
}

impl<Right, K, Input, RightKey, RightValue, JoinKey, Output, LeftKey, RightKeyFn, Project>
    PlanProperties
    for JoinStage<
        Right,
        K,
        Input,
        RightKey,
        RightValue,
        JoinKey,
        Output,
        LeftKey,
        RightKeyFn,
        Project,
    >
where
    Self: StageSpec,
{
    type Cardinality = ZeroOrOne;
    type InputPartition = <Self as StageSpec>::InputPartition;
    type OutputPartition = <Self as StageSpec>::OutputPartition;
}

/// Executable state for one stage of an arbitrary-length join region.
///
/// This is deliberately independent of query installation. Each instantiated
/// stage owns its typed relationship index; a later region executor can chain
/// as many differently typed states as its stage list requires.
pub(super) struct StageRuntimeState<K, I, RK, RV, JK, O, LeftKey, RightKeyFn, Project>
where
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
    right_rows: FxHashMap<RK, RV>,
    right_join_keys: FxHashMap<RK, JK>,
    join_to_right: FxHashMap<JK, Vec<RK>>,
    output_cache: FxHashMap<K, O>,
    left_key: LeftKey,
    right_key: RightKeyFn,
    project: Project,
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
        Self {
            left_rows: FxHashMap::default(),
            left_join_keys: FxHashMap::default(),
            join_to_left: FxHashMap::default(),
            right_rows: FxHashMap::default(),
            right_join_keys: FxHashMap::default(),
            join_to_right: FxHashMap::default(),
            output_cache: FxHashMap::default(),
            left_key,
            right_key,
            project,
        }
    }

    /// Apply one left event and return the resulting output changes.
    pub(super) fn apply_left_diff(&mut self, diff: &MapDiff<K, I>) -> Vec<MapDiff<K, O>> {
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

    /// Apply one right event and return changes for every affected left row.
    pub(super) fn apply_right_diff(&mut self, diff: &MapDiff<RK, RV>) -> Vec<MapDiff<K, O>> {
        let mut changed_join_keys = OrderedSet::default();
        let mut pending = vec![diff];
        while let Some(change) = pending.pop() {
            match change {
                MapDiff::Initial { entries } => {
                    changed_join_keys.extend(self.right_join_keys.values().cloned());
                    self.right_rows.clear();
                    self.right_join_keys.clear();
                    self.join_to_right.clear();
                    for (key, value) in entries {
                        self.upsert_right(key.clone(), value.clone(), &mut changed_join_keys);
                    }
                }
                MapDiff::Insert { key, value }
                | MapDiff::Update {
                    key,
                    new_value: value,
                    ..
                } => self.upsert_right(key.clone(), value.clone(), &mut changed_join_keys),
                MapDiff::Remove { key, .. } => {
                    self.remove_right(key, &mut changed_join_keys);
                }
                MapDiff::Batch { changes } => pending.extend(changes.iter().rev()),
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
        let join_key = (self.left_key)(&key, &value);
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

    fn upsert_right(&mut self, key: RK, value: RV, changed_join_keys: &mut OrderedSet<JK>) {
        let new_join_key = self.right_key.right_join_key(&key, &value);
        let old_join_key = self.right_join_keys.remove(&key);
        if old_join_key != new_join_key
            && let Some(old_join_key) = &old_join_key
        {
            remove_index_member(&mut self.join_to_right, old_join_key, &key);
            changed_join_keys.insert(old_join_key.clone());
        }

        if let Some(join_key) = new_join_key {
            if old_join_key.as_ref() != Some(&join_key) {
                add_index_member(&mut self.join_to_right, join_key.clone(), key.clone());
            }
            changed_join_keys.insert(join_key.clone());
            self.right_join_keys.insert(key.clone(), join_key);
            self.right_rows.insert(key, value);
        } else {
            self.right_rows.remove(&key);
        }
    }

    fn remove_right(&mut self, key: &RK, changed_join_keys: &mut OrderedSet<JK>) {
        if let Some(join_key) = self.right_join_keys.remove(key) {
            remove_index_member(&mut self.join_to_right, &join_key, key);
            changed_join_keys.insert(join_key);
        }
        self.right_rows.remove(key);
    }

    fn recompute_impacted(&mut self, impacted: &mut OrderedSet<K>) -> Vec<MapDiff<K, O>> {
        let mut changes = Vec::new();
        let mut right_matches = Vec::new();
        for key in impacted.drain() {
            let desired = self.left_rows.get(&key).and_then(|input| {
                right_matches.clear();
                if let Some(join_key) = self.left_join_keys.get(&key)
                    && let Some(right_keys) = self.join_to_right.get(join_key)
                {
                    right_matches.extend(right_keys.iter().filter_map(|right_key| {
                        self.right_rows
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

/// A left plan followed by an arbitrary heterogeneous list of join stages.
pub struct JoinRegion<Left, Stages, K, Input> {
    pub left: Left,
    pub stages: Stages,
    _types: PhantomData<fn() -> (K, Input)>,
}

impl<Left, Stages, K, Input> JoinRegion<Left, Stages, K, Input> {
    pub const fn new(left: Left, stages: Stages) -> Self {
        Self {
            left,
            stages,
            _types: PhantomData,
        }
    }
}

impl<Left, Stages, K, Input> PlanProperties for JoinRegion<Left, Stages, K, Input>
where
    Left: PlanProperties<OutputPartition = ByMapKey<K>>,
    Stages: StageList<K, Input>,
    K: Hash + Eq + CellValue,
    Input: CellValue,
{
    type Cardinality = ZeroOrOne;
    type InputPartition = Left::InputPartition;
    type OutputPartition = ByMapKey<K>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::map_query::properties::ExactlyOne;

    #[derive(Clone)]
    struct Source;
    impl PlanProperties for Source {
        type Cardinality = ExactlyOne;
        type InputPartition = ByMapKey<u32>;
        type OutputPartition = ByMapKey<u32>;
    }

    type Stage<I, O, R> = JoinStage<
        R,
        u32,
        I,
        u16,
        R,
        u64,
        O,
        fn(&u32, &I) -> u64,
        fn(&u16, &R) -> u64,
        DirectProject<fn(&u32, &I, &[(u16, R)]) -> O>,
    >;

    fn properties<P: PlanProperties<OutputPartition = ByMapKey<u32>>>() {}

    #[test]
    fn heterogeneous_three_stage_region_compiles() {
        type Stages = JCons<
            Stage<u8, u16, i8>,
            JCons<Stage<u16, u32, i16>, JCons<Stage<u32, u64, i32>, JNil>>,
        >;
        let _: Option<JoinRegion<Source, Stages, u32, u8>> = None;
        properties::<JoinRegion<Source, Stages, u32, u8>>();
    }

    #[test]
    fn heterogeneous_eight_stage_region_compiles() {
        type Stages = JCons<
            Stage<u8, u16, i8>,
            JCons<
                Stage<u16, u32, i16>,
                JCons<
                    Stage<u32, u64, i32>,
                    JCons<
                        Stage<u64, i8, i64>,
                        JCons<
                            Stage<i8, i16, u8>,
                            JCons<
                                Stage<i16, i32, u16>,
                                JCons<Stage<i32, i64, u32>, JCons<Stage<i64, usize, u64>, JNil>>,
                            >,
                        >,
                    >,
                >,
            >,
        >;
        let _: Option<JoinRegion<Source, Stages, u32, u8>> = None;
        properties::<JoinRegion<Source, Stages, u32, u8>>();
    }

    #[test]
    fn push_preserves_forward_order() {
        let stages = JNil.push(1_u8).push("second").push(3_u16);
        assert_eq!(stages.head, 1);
        assert_eq!(stages.tail.head, "second");
        assert_eq!(stages.tail.tail.head, 3);
    }

    #[test]
    fn collect_projection_then_map_and_filter_compose() {
        let projection = FilterProject::new(
            ThenMap::<_, _, usize>::new(
                CollectProject(|_: &u16, joined: &(String, Vec<u16>)| {
                    joined.0.len() + joined.1.len()
                }),
                |key: &u16, count: &usize| usize::from(*key) + count,
            ),
            |_: &u16, total: &usize| total.is_multiple_of(2),
        );
        let rights = [(1_u8, 10_u16), (2, 20)];
        assert_eq!(
            projection.project(&4, &"left".to_owned(), &rights),
            Some(10)
        );
        assert_eq!(projection.project(&3, &"left".to_owned(), &rights), None);
    }

    #[test]
    fn direct_projection_then_map_does_not_collect() {
        let projection = ThenMap::<_, _, usize>::new(
            DirectProject(|_: &u32, left: &usize, rights: &[(u8, usize)]| {
                *left + rights.iter().map(|(_, value)| value).sum::<usize>()
            }),
            |_: &u32, sum: &usize| sum * 2,
        );
        assert_eq!(projection.project(&0, &4, &[(1, 5), (2, 6)]), Some(30));
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn stage_runtime_tracks_optional_keys_moves_and_filter_disappearance() {
        type Right = (Option<u32>, i32);
        type Output = (bool, u32, Vec<i32>);

        let project = FilterProject::new(
            DirectProject(
                |_: &u32, left: &(u32, bool), rights: &[(u32, Right)]| -> Output {
                    (
                        left.1,
                        left.0,
                        rights.iter().map(|(_, right)| right.1).collect(),
                    )
                },
            ),
            |_: &u32, output: &Output| output.0,
        );
        let mut runtime = StageRuntimeState::new(
            |_: &u32, left: &(u32, bool)| left.0,
            crate::traits::OptionalRightKey(|_: &u32, right: &Right| right.0),
            project,
        );

        assert_eq!(
            runtime.apply_left_diff(&MapDiff::Insert {
                key: 1,
                value: (10, true),
            }),
            vec![MapDiff::Insert {
                key: 1,
                value: (true, 10, vec![]),
            }]
        );

        assert!(
            runtime
                .apply_right_diff(&MapDiff::Insert {
                    key: 7,
                    value: (None, 7),
                })
                .is_empty()
        );
        assert!(!runtime.right_rows.contains_key(&7));

        assert_eq!(
            runtime.apply_right_diff(&MapDiff::Update {
                key: 7,
                old_value: (None, 7),
                new_value: (Some(10), 7),
            }),
            vec![MapDiff::Update {
                key: 1,
                old_value: (true, 10, vec![]),
                new_value: (true, 10, vec![7]),
            }]
        );
        assert_eq!(
            runtime.apply_right_diff(&MapDiff::Update {
                key: 7,
                old_value: (Some(10), 7),
                new_value: (Some(20), 7),
            }),
            vec![MapDiff::Update {
                key: 1,
                old_value: (true, 10, vec![7]),
                new_value: (true, 10, vec![]),
            }]
        );
        assert_eq!(
            runtime.apply_left_diff(&MapDiff::Update {
                key: 1,
                old_value: (10, true),
                new_value: (20, true),
            }),
            vec![MapDiff::Update {
                key: 1,
                old_value: (true, 10, vec![]),
                new_value: (true, 20, vec![7]),
            }]
        );
        assert_eq!(
            runtime.apply_right_diff(&MapDiff::Update {
                key: 7,
                old_value: (Some(20), 7),
                new_value: (Some(20), 9),
            }),
            vec![MapDiff::Update {
                key: 1,
                old_value: (true, 20, vec![7]),
                new_value: (true, 20, vec![9]),
            }]
        );
        assert_eq!(
            runtime.apply_right_diff(&MapDiff::Update {
                key: 7,
                old_value: (Some(20), 9),
                new_value: (None, 9),
            }),
            vec![MapDiff::Update {
                key: 1,
                old_value: (true, 20, vec![9]),
                new_value: (true, 20, vec![]),
            }]
        );
        assert_eq!(
            runtime.apply_left_diff(&MapDiff::Update {
                key: 1,
                old_value: (20, true),
                new_value: (20, false),
            }),
            vec![MapDiff::Remove {
                key: 1,
                old_value: (true, 20, vec![]),
            }]
        );
        assert!(
            runtime
                .apply_right_diff(&MapDiff::Insert {
                    key: 7,
                    value: (Some(20), 11),
                })
                .is_empty()
        );
        assert_eq!(
            runtime.apply_left_diff(&MapDiff::Update {
                key: 1,
                old_value: (20, false),
                new_value: (20, true),
            }),
            vec![MapDiff::Insert {
                key: 1,
                value: (true, 20, vec![11]),
            }]
        );
        assert_eq!(
            runtime.apply_left_diff(&MapDiff::Remove {
                key: 1,
                old_value: (20, true),
            }),
            vec![MapDiff::Remove {
                key: 1,
                old_value: (true, 20, vec![11]),
            }]
        );
    }
}
