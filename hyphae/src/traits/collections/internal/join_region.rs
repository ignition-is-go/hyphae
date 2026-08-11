#![allow(dead_code)]

//! Static type substrate for an arbitrary-length left-join region.
//!
//! A join region is described and installed without tuples (and therefore
//! without tuple-arity limits), while every stage, right-root entry, and
//! projection remains statically dispatched.

use std::{
    any::TypeId,
    collections::hash_map::Entry,
    hash::{Hash, Hasher},
    marker::PhantomData,
    sync::{Arc, Mutex},
};

use rustc_hash::{FxHashMap, FxHasher};

use crate::{
    cell_map::MapDiff,
    map_query::{
        BuildQueryRuntime, MapDiffSink, MapQuery, compile_runtime_into,
        compiler::{CompileContext, DeferredPhysical, QUERY_POISONED_MESSAGE, QueryPoison},
        properties::{ByMapKey, Partition, PlanProperties, ZeroOrOne},
    },
    subscription::SubscriptionGuard,
    traits::{
        CellValue, ForeignKeyRelation, IdFor, OptionalRightKey, RequiredRightKey, RightJoinKey,
    },
};

use super::{
    join_runtime::{RelationIndex, RelationIndexStorage},
    ordered_set::OrderedSet,
};

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

/// Type information exposed by the final stage in a non-empty stage list.
#[doc(hidden)]
pub trait LastStage {
    type Input: CellValue;
    type RightKey: Hash + Eq + CellValue;
    type RightValue: CellValue;
}

impl<Right, K, Input, RK, RV, JK, Output, FL, FR, Project, Policy> LastStage
    for JCons<JoinStage<Right, K, Input, RK, RV, JK, Output, FL, FR, Project, Policy>, JNil>
where
    Input: CellValue,
    RK: Hash + Eq + CellValue,
    RV: CellValue,
{
    type Input = Input;
    type RightKey = RK;
    type RightValue = RV;
}

impl<Head, TailHead, TailTail> LastStage for JCons<Head, JCons<TailHead, TailTail>>
where
    JCons<TailHead, TailTail>: LastStage,
{
    type Input = <JCons<TailHead, TailTail> as LastStage>::Input;
    type RightKey = <JCons<TailHead, TailTail> as LastStage>::RightKey;
    type RightValue = <JCons<TailHead, TailTail> as LastStage>::RightValue;
}

/// Replace the final stage projection while preserving the stage spine.
#[doc(hidden)]
pub trait ReplaceLastProject<F, Output> {
    type Stages;

    fn replace_last_project(self, project: F) -> Self::Stages;
}

impl<Right, K, Input, RK, RV, JK, OldOutput, FL, FR, OldProject, Policy, F, Output>
    ReplaceLastProject<F, Output>
    for JCons<JoinStage<Right, K, Input, RK, RV, JK, OldOutput, FL, FR, OldProject, Policy>, JNil>
{
    type Stages = JCons<
        JoinStage<Right, K, Input, RK, RV, JK, Output, FL, FR, DirectProject<F>, Policy>,
        JNil,
    >;

    fn replace_last_project(self, project: F) -> Self::Stages {
        let JoinStage {
            right,
            left_key,
            right_key,
            index_policy,
            ..
        } = self.head;
        JCons {
            head: JoinStage::new(right, left_key, right_key, DirectProject(project))
                .with_index_policy(index_policy),
            tail: JNil,
        }
    }
}

impl<Head, TailHead, TailTail, F, Output> ReplaceLastProject<F, Output>
    for JCons<Head, JCons<TailHead, TailTail>>
where
    JCons<TailHead, TailTail>: ReplaceLastProject<F, Output>,
{
    type Stages = JCons<Head, <JCons<TailHead, TailTail> as ReplaceLastProject<F, Output>>::Stages>;

    fn replace_last_project(self, project: F) -> Self::Stages {
        JCons {
            head: self.head,
            tail: self.tail.replace_last_project(project),
        }
    }
}

/// Map the successful output of the final stage without adding a plan node.
#[doc(hidden)]
pub trait MapLast<F, Output> {
    type Stages;

    fn map_last(self, map: F) -> Self::Stages;
}

impl<Right, K, Input, RK, RV, JK, Intermediate, FL, FR, Project, Policy, F, Output>
    MapLast<F, Output>
    for JCons<JoinStage<Right, K, Input, RK, RV, JK, Intermediate, FL, FR, Project, Policy>, JNil>
{
    type Stages = JCons<
        JoinStage<
            Right,
            K,
            Input,
            RK,
            RV,
            JK,
            Output,
            FL,
            FR,
            ThenMap<Project, F, Intermediate>,
            Policy,
        >,
        JNil,
    >;

    fn map_last(self, map: F) -> Self::Stages {
        let JoinStage {
            right,
            left_key,
            right_key,
            project,
            index_policy,
            ..
        } = self.head;
        JCons {
            head: JoinStage::new(right, left_key, right_key, ThenMap::new(project, map))
                .with_index_policy(index_policy),
            tail: JNil,
        }
    }
}

impl<Head, TailHead, TailTail, F, Output> MapLast<F, Output>
    for JCons<Head, JCons<TailHead, TailTail>>
where
    JCons<TailHead, TailTail>: MapLast<F, Output>,
{
    type Stages = JCons<Head, <JCons<TailHead, TailTail> as MapLast<F, Output>>::Stages>;

    fn map_last(self, map: F) -> Self::Stages {
        JCons {
            head: self.head,
            tail: self.tail.map_last(map),
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

#[allow(clippy::redundant_pub_crate)]
pub(crate) fn collect_matches<K, Input, RK, RV>(
    _: &K,
    input: &Input,
    rights: &[(RK, RV)],
) -> (Input, Vec<RV>)
where
    Input: Clone,
    RV: Clone,
{
    (
        input.clone(),
        rights.iter().map(|(_, value)| value.clone()).collect(),
    )
}

pub fn map_key<K: Clone, V>(key: &K, _: &V) -> K {
    key.clone()
}

pub fn foreign_map_key<Rel, RK, K>(_: &RK, value: &Rel::Child) -> Option<K>
where
    Rel: ForeignKeyRelation,
    Rel::ForeignKey: IdFor<Rel::Parent, MapKey = K>,
{
    Rel::foreign_key(value).map(|foreign_key| foreign_key.map_key())
}

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

/// Keep a stage's relationship index private to that stage.
#[derive(Debug, Clone, Default)]
pub struct OwnedIndex;

/// Share the physical relationship index for raw roots bearing `Rel`.
#[derive(Debug, Clone, Copy, Default)]
pub struct SharedRelationIndex<Rel>(PhantomData<fn() -> Rel>);

impl<Rel> SharedRelationIndex<Rel> {
    #[must_use]
    pub const fn new() -> Self {
        Self(PhantomData)
    }
}

trait IndexPolicy<RK, RV, JK>: Send + Sync + 'static
where
    RK: Hash + Eq + CellValue,
    RV: CellValue,
    JK: Hash + Eq + CellValue,
{
    type Storage: RelationIndexStorage<RK, RV, JK> + Clone;
    type Binding: Clone + Send + Sync + 'static;

    fn prepare(cx: &CompileContext, shareable: bool) -> (Self::Storage, Option<Self::Binding>);
    fn maintains(binding: Option<&Self::Binding>) -> bool;
    fn install<Right, Sink>(
        right: Right,
        cx: &mut CompileContext,
        binding: Option<Self::Binding>,
        sink: Sink,
    ) -> Vec<SubscriptionGuard>
    where
        Right: MapQuery<Key = RK, Value = RV>,
        Sink: MapDiffSink<RK, RV>;
}

impl<RK, RV, JK> IndexPolicy<RK, RV, JK> for OwnedIndex
where
    RK: Hash + Eq + CellValue,
    RV: CellValue,
    JK: Hash + Eq + CellValue,
{
    type Storage = DeferredPhysical<RelationIndex<RK, RV, JK>>;
    type Binding = ();

    fn prepare(_: &CompileContext, _: bool) -> (Self::Storage, Option<Self::Binding>) {
        (DeferredPhysical::default(), None)
    }

    fn maintains(_: Option<&Self::Binding>) -> bool {
        true
    }

    fn install<Right, Sink>(
        right: Right,
        cx: &mut CompileContext,
        _: Option<Self::Binding>,
        sink: Sink,
    ) -> Vec<SubscriptionGuard>
    where
        Right: MapQuery<Key = RK, Value = RV>,
        Sink: MapDiffSink<RK, RV>,
    {
        compile_runtime_into(right, cx, sink)
    }
}

impl<RK, RV, JK, Rel> IndexPolicy<RK, RV, JK> for SharedRelationIndex<Rel>
where
    RK: Hash + Eq + CellValue,
    RV: CellValue,
    JK: Hash + Eq + CellValue,
    Rel: Send + Sync + 'static,
{
    type Storage = DeferredPhysical<RelationIndex<RK, RV, JK>>;
    type Binding = DeferredPhysical<RelationIndex<RK, RV, JK>>;

    fn prepare(cx: &CompileContext, shareable: bool) -> (Self::Storage, Option<Self::Binding>) {
        let index = cx.prepare_relationship_index();
        let binding = shareable.then(|| index.clone());
        (index, binding)
    }

    fn maintains(binding: Option<&Self::Binding>) -> bool {
        binding.is_none_or(DeferredPhysical::maintains_index)
    }

    fn install<Right, Sink>(
        right: Right,
        cx: &mut CompileContext,
        binding: Option<Self::Binding>,
        sink: Sink,
    ) -> Vec<SubscriptionGuard>
    where
        Right: MapQuery<Key = RK, Value = RV>,
        Sink: MapDiffSink<RK, RV>,
    {
        if let Some(index) = binding {
            cx.with_root_relation_index(TypeId::of::<Rel>(), index, |cx| {
                compile_runtime_into(right, cx, sink)
            })
        } else {
            compile_runtime_into(right, cx, sink)
        }
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
    Policy = OwnedIndex,
> {
    pub right: Right,
    pub left_key: LeftKey,
    pub right_key: RightKeyFn,
    pub project: Project,
    pub index_policy: Policy,
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
            index_policy: OwnedIndex,
            _types: PhantomData,
        }
    }

    pub fn with_index_policy<Policy>(
        self,
        index_policy: Policy,
    ) -> JoinStage<
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
        Policy,
    > {
        JoinStage {
            right: self.right,
            left_key: self.left_key,
            right_key: self.right_key,
            project: self.project,
            index_policy,
            _types: PhantomData,
        }
    }
}

impl<Right, K, Input, RightKey, RightValue, JoinKey, Output, LeftKey, RightKeyFn, Project, Policy>
    StageSpec
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
        Policy,
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
    Policy: Send + Sync + 'static,
{
    type Key = K;
    type Input = Input;
    type Output = Output;
    type InputPartition = ByMapKey<K>;
    type OutputPartition = ByMapKey<K>;
}

impl<Right, K, Input, RightKey, RightValue, JoinKey, Output, LeftKey, RightKeyFn, Project, Policy>
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
        Policy,
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

fn batch_has_unique_atomic_keys<K, V>(changes: &[MapDiff<K, V>]) -> bool
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

/// Dual-mode whole-region executor. Tiny events use the original single
/// runtime without extra hashing. Promotion is one-way; afterwards every map
/// key owns one persistent shard for the entire heterogeneous stage spine.
struct RegionRouter<Runtime, K, Input> {
    sequential: Option<Runtime>,
    shards: Option<Vec<Runtime>>,
    key_sequence: FxHashMap<K, u64>,
    next_sequence: u64,
    shard_count: usize,
    promotion_work: usize,
    parallel_active: bool,
    /// Terminal fail-stop state. A projection/apply unwind can leave typed shard
    /// runtimes or a maintained right index partially changed, so the region is
    /// quarantined permanently rather than exposed for recovery.
    poisoned: bool,
    query_poison: QueryPoison,
    #[cfg(test)]
    test_dispatch: Option<crate::map_query::compiler::TestRegionDispatch>,
    #[cfg(test)]
    last_left_workers: Vec<String>,
    _input: PhantomData<fn() -> Input>,
}

const DEFAULT_PROMOTION_WORK: usize = 8_192;
// The frozen four-stage workload clears the strict 1.5x confidence-bound gate
// at the first 200k-cost batch. The wide band also prevents oscillation.
const PARALLEL_REGION_WORK_ENTER: usize = 200_000;
const PARALLEL_REGION_WORK_EXIT: usize = 96_000;

#[allow(clippy::missing_const_for_fn)]
fn configured_shards() -> usize {
    #[cfg(feature = "scheduler")]
    let count = crate::executor::configured_worker_threads().max(1);
    #[cfg(not(feature = "scheduler"))]
    let count = 1;
    count
}

const fn configured_promotion_work() -> usize {
    DEFAULT_PROMOTION_WORK
}

fn diff_work<K, V>(diff: &MapDiff<K, V>) -> usize {
    match diff {
        MapDiff::Initial { entries } => entries.len(),
        MapDiff::Batch { changes } => changes.iter().map(diff_work).sum(),
        _ => 1,
    }
}

const fn diff_key<K, V>(diff: &MapDiff<K, V>) -> Option<&K> {
    match diff {
        MapDiff::Insert { key, .. } | MapDiff::Update { key, .. } | MapDiff::Remove { key, .. } => {
            Some(key)
        }
        MapDiff::Initial { .. } | MapDiff::Batch { .. } => None,
    }
}

#[allow(
    clippy::arithmetic_side_effects,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::items_after_statements,
    clippy::too_many_lines
)]
impl<Runtime, K, Input> RegionRouter<Runtime, K, Input>
where
    K: Hash + Eq + CellValue,
    Input: CellValue,
    Runtime: RuntimeStages<K, Input>
        + EmptyShardRuntime
        + HeadInputSnapshot<K, Input>
        + RuntimeStageCost
        + Send,
{
    fn new(runtime: Runtime) -> Self {
        Self {
            sequential: Some(runtime),
            shards: None,
            key_sequence: FxHashMap::default(),
            next_sequence: 0,
            shard_count: configured_shards(),
            promotion_work: configured_promotion_work(),
            parallel_active: false,
            poisoned: false,
            query_poison: QueryPoison::default(),
            #[cfg(test)]
            test_dispatch: None,
            #[cfg(test)]
            last_left_workers: Vec::new(),
            _input: PhantomData,
        }
    }

    fn with_query_poison(runtime: Runtime, query_poison: QueryPoison) -> Self {
        let mut router = Self::new(runtime);
        router.query_poison = query_poison;
        router
    }

    #[cfg(test)]
    fn with_config(runtime: Runtime, shard_count: usize, promotion_work: usize) -> Self {
        let mut router = Self::new(runtime);
        router.shard_count = shard_count.max(1);
        router.promotion_work = promotion_work.max(1);
        router
    }

    #[cfg(test)]
    fn with_test_config(
        runtime: Runtime,
        query_poison: QueryPoison,
        config: crate::map_query::compiler::TestRegionConfig,
    ) -> Self {
        Self {
            sequential: Some(runtime),
            shards: None,
            key_sequence: FxHashMap::default(),
            next_sequence: 0,
            shard_count: config.shards.max(1),
            promotion_work: config.promote_after.max(1),
            parallel_active: false,
            poisoned: false,
            query_poison,
            test_dispatch: Some(config.dispatch),
            last_left_workers: Vec::new(),
            _input: PhantomData,
        }
    }

    #[allow(clippy::unused_self)]
    const fn test_configured(&self) -> bool {
        #[cfg(test)]
        {
            self.test_dispatch.is_some()
        }
        #[cfg(not(test))]
        {
            false
        }
    }

    fn shard_for(key: &K, count: usize) -> usize {
        let mut hasher = FxHasher::default();
        key.hash(&mut hasher);
        let count = u64::try_from(count.max(1)).unwrap_or(1);
        let index = hasher.finish().checked_rem(count).unwrap_or(0);
        usize::try_from(index).unwrap_or(0)
    }

    fn remember(&mut self, diff: &MapDiff<K, Input>) {
        match diff {
            MapDiff::Initial { entries } => {
                self.key_sequence.clear();
                self.next_sequence = 0;
                for (key, _) in entries {
                    self.key_sequence.insert(key.clone(), self.next_sequence);
                    self.next_sequence += 1;
                }
            }
            MapDiff::Insert { key, .. } | MapDiff::Update { key, .. } => {
                if !self.key_sequence.contains_key(key) {
                    self.key_sequence.insert(key.clone(), self.next_sequence);
                    self.next_sequence += 1;
                }
            }
            MapDiff::Remove { key, .. } => {
                self.key_sequence.remove(key);
            }
            MapDiff::Batch { changes } => changes.iter().for_each(|change| self.remember(change)),
        }
    }

    fn merge_order(&self, diff: &MapDiff<K, Input>) -> FxHashMap<K, u64> {
        let mut order = FxHashMap::default();
        let mut next = 0;
        fn remember<K: Hash + Eq + Clone>(key: &K, order: &mut FxHashMap<K, u64>, next: &mut u64) {
            if !order.contains_key(key) {
                order.insert(key.clone(), *next);
                *next = next.saturating_add(1);
            }
        }
        fn visit<K: Hash + Eq + Clone, V>(
            diff: &MapDiff<K, V>,
            existing: &mut FxHashMap<K, u64>,
            next_sequence: &mut u64,
            order: &mut FxHashMap<K, u64>,
            next: &mut u64,
        ) {
            match diff {
                MapDiff::Initial { entries } => {
                    // An Initial has its own deterministic event-local order:
                    // the live rows in saved sequence, followed by new rows in
                    // entry order. Overwrite earlier tiebreaks because tagged
                    // output ordinals keep separate input events apart.
                    let mut old: Vec<_> = existing.iter().collect();
                    old.sort_by_key(|(_, sequence)| **sequence);
                    let mut initial_next = 0;
                    for (key, _) in old {
                        order.insert(key.clone(), initial_next);
                        initial_next = initial_next.saturating_add(1);
                    }
                    for (key, _) in entries {
                        if !order.contains_key(key) {
                            order.insert(key.clone(), initial_next);
                            initial_next = initial_next.saturating_add(1);
                        }
                    }
                    existing.clear();
                    *next_sequence = 0;
                    for (key, _) in entries {
                        existing.insert(key.clone(), *next_sequence);
                        *next_sequence = next_sequence.saturating_add(1);
                    }
                }
                MapDiff::Insert { key, .. } | MapDiff::Update { key, .. } => {
                    remember(key, order, next);
                    if !existing.contains_key(key) {
                        existing.insert(key.clone(), *next_sequence);
                        *next_sequence = next_sequence.saturating_add(1);
                    }
                }
                MapDiff::Remove { key, .. } => {
                    remember(key, order, next);
                    existing.remove(key);
                }
                MapDiff::Batch { changes } => changes.iter().for_each(|change| {
                    visit(change, existing, next_sequence, order, next);
                }),
            }
        }
        let mut existing = self.key_sequence.clone();
        let mut next_sequence = existing
            .values()
            .copied()
            .max()
            .map_or(0, |value| value.saturating_add(1));
        visit(
            diff,
            &mut existing,
            &mut next_sequence,
            &mut order,
            &mut next,
        );
        order
    }

    fn promote(&mut self) {
        if self.shards.is_some() {
            return;
        }
        let count = self.shard_count.max(1);
        let sequential = self
            .sequential
            .take()
            .expect("sequential runtime exists before promotion");
        let mut shards: Vec<_> = (0..count).map(|_| sequential.empty_shard()).collect();
        // Replay in stable source order. Outputs are intentionally discarded.
        let mut keys: Vec<_> = self.key_sequence.iter().collect();
        keys.sort_by_key(|(_, sequence)| **sequence);
        for (key, _) in keys {
            if let Some(value) = sequential.head_input(key) {
                let shard = Self::shard_for(key, count);
                let _ = shards[shard].apply_left_diff(&MapDiff::Insert {
                    key: key.clone(),
                    value,
                });
            }
        }
        self.shards = Some(shards);
    }

    fn order_changes<Output: CellValue>(
        order: &FxHashMap<K, u64>,
        changes: &mut [MapDiff<K, Output>],
    ) {
        changes.sort_by_key(|change| {
            diff_key(change)
                .and_then(|key| order.get(key))
                .copied()
                .unwrap_or(u64::MAX)
        });
    }

    fn route_diff(
        diff: &MapDiff<K, Input>,
        shard_count: usize,
        next_ordinal: &mut u64,
        routed: &mut [Vec<(u64, MapDiff<K, Input>)>],
    ) {
        match diff {
            MapDiff::Batch { changes } => {
                for change in changes {
                    Self::route_diff(change, shard_count, next_ordinal, routed);
                }
            }
            MapDiff::Initial { entries } => {
                let ordinal = *next_ordinal;
                *next_ordinal = next_ordinal.saturating_add(1);
                let mut partitioned = vec![Vec::new(); shard_count];
                for (key, value) in entries {
                    partitioned[Self::shard_for(key, shard_count)]
                        .push((key.clone(), value.clone()));
                }
                for (id, entries) in partitioned.into_iter().enumerate() {
                    routed[id].push((ordinal, MapDiff::Initial { entries }));
                }
            }
            other => {
                let ordinal = *next_ordinal;
                *next_ordinal = next_ordinal.saturating_add(1);
                let key = diff_key(other).expect("non-container diff has a key");
                routed[Self::shard_for(key, shard_count)].push((ordinal, other.clone()));
            }
        }
    }

    fn extend_flat<Output: CellValue>(
        output: &mut Vec<MapDiff<K, Output>>,
        changes: Vec<MapDiff<K, Output>>,
    ) {
        for change in changes {
            match change {
                MapDiff::Batch { changes } => Self::extend_flat(output, changes),
                leaf => output.push(leaf),
            }
        }
    }

    fn event_orders(&self, diff: &MapDiff<K, Input>) -> Vec<FxHashMap<K, u64>> {
        fn visit<K: Hash + Eq + Clone, V>(
            diff: &MapDiff<K, V>,
            existing: &mut FxHashMap<K, u64>,
            next_sequence: &mut u64,
            orders: &mut Vec<FxHashMap<K, u64>>,
        ) {
            if let MapDiff::Batch { changes } = diff {
                for change in changes {
                    visit(change, existing, next_sequence, orders);
                }
                return;
            }
            let mut event = FxHashMap::default();
            match diff {
                MapDiff::Initial { entries } => {
                    let mut old: Vec<_> = existing.iter().collect();
                    old.sort_by_key(|(_, sequence)| **sequence);
                    let mut rank: u64 = 0;
                    for (key, _) in old {
                        event.insert(key.clone(), rank);
                        rank = rank.saturating_add(1);
                    }
                    for (key, _) in entries {
                        if !event.contains_key(key) {
                            event.insert(key.clone(), rank);
                            rank = rank.saturating_add(1);
                        }
                    }
                    existing.clear();
                    *next_sequence = 0;
                    for (key, _) in entries {
                        existing.insert(key.clone(), *next_sequence);
                        *next_sequence = next_sequence.saturating_add(1);
                    }
                }
                MapDiff::Insert { key, .. } | MapDiff::Update { key, .. } => {
                    event.insert(key.clone(), 0);
                    if !existing.contains_key(key) {
                        existing.insert(key.clone(), *next_sequence);
                        *next_sequence = next_sequence.saturating_add(1);
                    }
                }
                MapDiff::Remove { key, .. } => {
                    event.insert(key.clone(), 0);
                    existing.remove(key);
                }
                MapDiff::Batch { .. } => return,
            }
            orders.push(event);
        }
        let mut existing = self.key_sequence.clone();
        let mut next_sequence = existing
            .values()
            .copied()
            .max()
            .map_or(0, |value| value.saturating_add(1));
        let mut orders = Vec::new();
        visit(diff, &mut existing, &mut next_sequence, &mut orders);
        orders
    }

    fn apply_left_eventwise(
        &mut self,
        diff: &MapDiff<K, Input>,
        output: &mut Vec<MapDiff<K, Runtime::Output>>,
    ) {
        if let MapDiff::Batch { changes } = diff {
            for change in changes {
                self.apply_left_eventwise(change, output);
            }
            return;
        }
        let order = self.merge_order(diff);
        let changes = self
            .sequential
            .as_mut()
            .expect("sequential mode")
            .apply_left_diff(diff);
        let mut flat = Vec::new();
        Self::extend_flat(&mut flat, changes);
        Self::order_changes(&order, &mut flat);
        output.extend(flat);
        self.remember(diff);
    }

    fn apply_left(&mut self, diff: &MapDiff<K, Input>) -> Vec<MapDiff<K, Runtime::Output>> {
        let batch_is_unique = match diff {
            MapDiff::Batch { changes } => Some(batch_has_unique_atomic_keys(changes)),
            _ => None,
        };
        let non_unique_batch = matches!(batch_is_unique, Some(false));
        if self.shards.is_none() && self.shard_count <= 1 && !self.test_configured() {
            #[cfg(feature = "region-calibration")]
            crate::region_calibration::left_serial_dispatch();
            if non_unique_batch {
                let mut changes = Vec::new();
                self.apply_left_eventwise(diff, &mut changes);
                return vec![MapDiff::Batch { changes }];
            }
            if matches!(diff, MapDiff::Initial { .. }) {
                let order = self.merge_order(diff);
                let mut output = self
                    .sequential
                    .as_mut()
                    .expect("sequential mode")
                    .apply_left_diff(diff);
                Self::order_changes(&order, &mut output);
                self.remember(diff);
                return output;
            }
            self.remember(diff);
            return self
                .sequential
                .as_mut()
                .expect("sequential mode")
                .apply_left_diff(diff);
        }
        let estimated_work = diff_work(diff).saturating_mul(Runtime::COST_UNITS);
        let promotion_warranted =
            diff_work(diff) >= self.promotion_work || estimated_work >= PARALLEL_REGION_WORK_ENTER;
        if self.shards.is_none()
            && ((self.shard_count <= 1 && !self.test_configured()) || !promotion_warranted)
        {
            #[cfg(feature = "region-calibration")]
            crate::region_calibration::left_serial_dispatch();
            if non_unique_batch {
                let mut changes = Vec::new();
                self.apply_left_eventwise(diff, &mut changes);
                return vec![MapDiff::Batch { changes }];
            }
            if matches!(diff, MapDiff::Initial { .. }) {
                let order = self.merge_order(diff);
                let mut output = self
                    .sequential
                    .as_mut()
                    .expect("sequential mode")
                    .apply_left_diff(diff);
                Self::order_changes(&order, &mut output);
                self.remember(diff);
                return output;
            }
            self.remember(diff);
            return self
                .sequential
                .as_mut()
                .expect("sequential mode")
                .apply_left_diff(diff);
        }
        if self.shards.is_none() {
            self.promote();
        }

        let order = self.merge_order(diff);
        let event_orders = non_unique_batch.then(|| self.event_orders(diff));
        let preserve_batch = batch_is_unique.is_some();
        let unique_batch = batch_is_unique.unwrap_or(false);

        let shards = self.shards.as_mut().expect("promoted");
        let mut routed = vec![Vec::new(); shards.len()];
        let mut next_ordinal = 0;
        Self::route_diff(diff, shards.len(), &mut next_ordinal, &mut routed);

        let hysteresis_wants_parallel = if self.parallel_active {
            estimated_work >= PARALLEL_REGION_WORK_EXIT
        } else {
            estimated_work >= PARALLEL_REGION_WORK_ENTER
        };
        let shard_work: Vec<_> = routed
            .iter()
            .map(|changes| {
                changes.iter().fold(0_usize, |work, (_, change)| {
                    work.saturating_add(diff_work(change))
                })
            })
            .collect();
        let active_shards = shard_work.iter().filter(|work| **work != 0).count();
        let max_shard_work = shard_work.iter().copied().max().unwrap_or(0);
        let balanced = active_shards > 1
            && max_shard_work.saturating_mul(4) <= diff_work(diff).saturating_mul(3);
        #[cfg(not(feature = "scheduler"))]
        let _ = (hysteresis_wants_parallel, balanced);
        #[cfg(all(test, not(target_arch = "wasm32")))]
        let forced_pool = match &self.test_dispatch {
            Some(crate::map_query::compiler::TestRegionDispatch::InjectedRayon(pool)) => {
                Some(Arc::clone(pool))
            }
            _ => None,
        };
        #[cfg(all(test, not(target_arch = "wasm32")))]
        let force_parallel = forced_pool.is_some();
        #[cfg(not(all(test, not(target_arch = "wasm32"))))]
        let force_parallel = false;
        #[cfg(all(test, feature = "scheduler", not(target_arch = "wasm32")))]
        let allow_production_dispatch = self.test_dispatch.is_none();
        #[cfg(all(not(test), feature = "scheduler", not(target_arch = "wasm32")))]
        let allow_production_dispatch = true;
        #[cfg(all(feature = "scheduler", not(target_arch = "wasm32")))]
        let production_resources_available = allow_production_dispatch
            && !force_parallel
            && hysteresis_wants_parallel
            && balanced
            && crate::executor::worker_pool().is_some();
        #[cfg(not(all(feature = "scheduler", not(target_arch = "wasm32"))))]
        let production_resources_available = false;
        let resources_available = force_parallel || production_resources_available;
        #[cfg(feature = "region-calibration")]
        let was_parallel = self.parallel_active;
        self.parallel_active = resources_available;
        #[cfg(feature = "region-calibration")]
        match (was_parallel, self.parallel_active) {
            (false, true) => crate::region_calibration::inactive_to_parallel(),
            (true, false) => crate::region_calibration::parallel_to_inactive(),
            _ => {}
        }
        let run_parallel = self.parallel_active;

        let process = |(shard_id, (shard, changes)): (
            usize,
            (&mut Runtime, Vec<(u64, MapDiff<K, Input>)>),
        )| {
            let mut tagged = Vec::new();
            if unique_batch && !changes.is_empty() {
                let batch = MapDiff::Batch {
                    changes: changes.into_iter().map(|(_, change)| change).collect(),
                };
                let mut flat = Vec::new();
                Self::extend_flat(&mut flat, shard.apply_left_diff(&batch));
                for (local, change) in flat.into_iter().enumerate() {
                    let ordinal = diff_key(&change)
                        .and_then(|key| order.get(key))
                        .copied()
                        .unwrap_or(u64::MAX);
                    tagged.push((ordinal, local, shard_id, change));
                }
            } else {
                for (ordinal, change) in changes {
                    let mut flat = Vec::new();
                    Self::extend_flat(&mut flat, shard.apply_left_diff(&change));
                    for (local, output) in flat.into_iter().enumerate() {
                        tagged.push((ordinal, local, shard_id, output));
                    }
                }
            }
            let worker = if cfg!(test) {
                std::thread::current().name().map(str::to_owned)
            } else {
                None
            };
            (tagged, worker)
        };

        #[cfg(all(any(feature = "scheduler", test), not(target_arch = "wasm32")))]
        let per_shard = if run_parallel {
            #[cfg(test)]
            let pool = forced_pool.as_deref().or_else(|| {
                #[cfg(feature = "scheduler")]
                {
                    crate::executor::worker_pool()
                }
                #[cfg(not(feature = "scheduler"))]
                {
                    None
                }
            });
            #[cfg(not(test))]
            let pool = crate::executor::worker_pool();
            if let Some(pool) = pool {
                #[cfg(feature = "region-calibration")]
                crate::region_calibration::left_parallel_dispatch();
                use rayon::prelude::*;
                pool.install(|| {
                    shards
                        .par_iter_mut()
                        .zip(routed.into_par_iter())
                        .enumerate()
                        .map(process)
                        .collect::<Vec<_>>()
                })
            } else {
                #[cfg(feature = "region-calibration")]
                crate::region_calibration::left_serial_dispatch();
                shards
                    .iter_mut()
                    .zip(routed)
                    .enumerate()
                    .map(process)
                    .collect()
            }
        } else {
            #[cfg(feature = "region-calibration")]
            crate::region_calibration::left_serial_dispatch();
            shards
                .iter_mut()
                .zip(routed)
                .enumerate()
                .map(process)
                .collect()
        };
        #[cfg(not(all(any(feature = "scheduler", test), not(target_arch = "wasm32"))))]
        let per_shard: Vec<_> = {
            let _ = run_parallel;
            #[cfg(feature = "region-calibration")]
            crate::region_calibration::left_serial_dispatch();
            shards
                .iter_mut()
                .zip(routed)
                .enumerate()
                .map(process)
                .collect()
        };

        #[cfg(test)]
        {
            self.last_left_workers = per_shard
                .iter()
                .filter_map(|(_, worker)| worker.clone())
                .collect();
        }
        let mut tagged: Vec<_> = per_shard
            .into_iter()
            .flat_map(|(changes, _)| changes)
            .collect();
        tagged.sort_by_key(|(ordinal, local, shard, change)| {
            let key_order = if unique_batch || event_orders.is_none() {
                diff_key(change)
                    .and_then(|key| order.get(key))
                    .copied()
                    .unwrap_or(u64::MAX)
            } else {
                usize::try_from(*ordinal)
                    .ok()
                    .and_then(|index| event_orders.as_ref().and_then(|orders| orders.get(index)))
                    .and_then(|event| diff_key(change).and_then(|key| event.get(key)))
                    .copied()
                    .unwrap_or(u64::MAX)
            };
            (*ordinal, key_order, *local, *shard)
        });
        let output = tagged.into_iter().map(|(_, _, _, change)| change).collect();
        self.remember(diff);
        if preserve_batch {
            vec![MapDiff::Batch { changes: output }]
        } else {
            output
        }
    }

    fn right_leaves<'a, RK, RV>(diff: &'a MapDiff<RK, RV>, leaves: &mut Vec<&'a MapDiff<RK, RV>>) {
        if let MapDiff::Batch { changes } = diff {
            for change in changes {
                Self::right_leaves(change, leaves);
            }
        } else {
            leaves.push(diff);
        }
    }

    #[allow(clippy::branches_sharing_code)]
    fn apply_right<Location, RK, RV>(
        &mut self,
        diff: &MapDiff<RK, RV>,
        maintain: bool,
    ) -> Vec<MapDiff<K, Runtime::Output>>
    where
        RK: Hash + Eq + CellValue,
        RV: CellValue,
        Runtime: RightRoot<Location, K, Input, RK, RV>,
    {
        // Canonical router traces follow stable left-source order in every
        // execution mode. Raw stage-kernel bucket order is a hash/index
        // implementation detail and cannot be reconstructed across shards.
        if self.shards.is_none() && self.shard_count <= 1 && !self.test_configured() {
            let mut output = self
                .sequential
                .as_mut()
                .expect("sequential mode")
                .apply_right_root_diff_policy(diff, maintain);
            Self::order_changes(&self.key_sequence, &mut output);
            return output;
        }
        if self.shards.is_none() && self.shard_count > 1 && diff_work(diff) >= self.promotion_work {
            self.promote();
        }
        if let Some(shards) = self.shards.as_mut() {
            let order = &self.key_sequence;
            let preserve_batch = matches!(diff, MapDiff::Batch { .. });
            let mut leaves = Vec::new();
            Self::right_leaves(diff, &mut leaves);
            let mut output = Vec::new();
            // Advance the shared physical index one source member at a time.
            // Every observer shard therefore reads the same snapshot that the
            // sequential runtime used for this phase before the next member.
            for leaf in leaves {
                let mut phase = Vec::new();
                for (id, shard) in shards.iter_mut().enumerate() {
                    Self::extend_flat(
                        &mut phase,
                        shard.apply_right_root_diff_policy(leaf, maintain && id == 0),
                    );
                }
                Self::order_changes(order, &mut phase);
                output.extend(phase);
            }
            if preserve_batch {
                vec![MapDiff::Batch { changes: output }]
            } else {
                output
            }
        } else {
            if !matches!(diff, MapDiff::Batch { .. }) {
                let mut output = self
                    .sequential
                    .as_mut()
                    .expect("sequential mode")
                    .apply_right_root_diff_policy(diff, maintain);
                Self::order_changes(&self.key_sequence, &mut output);
                return output;
            }
            let order = &self.key_sequence;
            let preserve_batch = true;
            let mut leaves = Vec::new();
            Self::right_leaves(diff, &mut leaves);
            let runtime = self.sequential.as_mut().expect("sequential mode");
            let mut output = Vec::new();
            for leaf in leaves {
                let mut phase = Vec::new();
                Self::extend_flat(
                    &mut phase,
                    runtime.apply_right_root_diff_policy(leaf, maintain),
                );
                Self::order_changes(order, &mut phase);
                output.extend(phase);
            }
            if preserve_batch {
                vec![MapDiff::Batch { changes: output }]
            } else {
                output
            }
        }
    }
}

/// Executes one complete left or right callback transaction while holding the
/// region mutex. This provides fail-stop publication atomicity: output is
/// returned to the callback only after the whole region apply succeeds. It is
/// deliberately not recovery-and-continue, and cannot roll back independent
/// sinks that an enclosing query already published before this callback began.
#[allow(clippy::panic)]
fn region_transaction<Runtime, K, Input, Output>(
    state: &Arc<Mutex<RegionRouter<Runtime, K, Input>>>,
    apply: impl FnOnce(&mut RegionRouter<Runtime, K, Input>) -> Output,
) -> Output {
    let mut router = state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if router.poisoned {
        drop(router);
        std::panic::panic_any(QUERY_POISONED_MESSAGE);
    }

    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| apply(&mut router))) {
        Ok(output) => {
            drop(router);
            output
        }
        Err(payload) => {
            // Rayon propagates only after its scope has joined every worker, so
            // no shard can remain active by the time this catch arm executes.
            router.query_poison.poison();
            router.poisoned = true;
            router.parallel_active = false;
            #[cfg(test)]
            router.last_left_workers.clear();
            drop(router);
            std::panic::resume_unwind(payload);
        }
    }
}

/// Select the first right root in a runtime-stage list.
pub(super) struct Here;

/// Select a right root in the tail; nesting counts stages from the front.
pub(super) struct There<Location>(PhantomData<fn() -> Location>);

/// Direct entry from one right root into its selected stage.
///
/// `Here` updates the head and propagates its output through the tail. A
/// `There<L>` implementation delegates directly to the tail, so earlier stages
/// are not re-executed when a later right root changes.
pub(super) trait RightRoot<Location, K, Input, RK, RV>: RuntimeStages<K, Input>
where
    K: Hash + Eq + CellValue,
    Input: CellValue,
    RK: Hash + Eq + CellValue,
    RV: CellValue,
{
    fn apply_right_root_diff(&mut self, diff: &MapDiff<RK, RV>) -> Vec<MapDiff<K, Self::Output>> {
        self.apply_right_root_diff_policy(diff, true)
    }

    fn apply_right_root_diff_policy(
        &mut self,
        diff: &MapDiff<RK, RV>,
        maintain: bool,
    ) -> Vec<MapDiff<K, Self::Output>>;
}

impl<K, Input, RK, RV, Head, Tail> RightRoot<Here, K, Input, RK, RV> for JCons<Head, Tail>
where
    K: Hash + Eq + CellValue,
    Input: CellValue,
    RK: Hash + Eq + CellValue,
    RV: CellValue,
    Head: ExecutableStage<Key = K, Input = Input, RightKey = RK, RightValue = RV>,
    Tail: RuntimeStages<K, Head::Output>,
{
    fn apply_right_root_diff_policy(
        &mut self,
        diff: &MapDiff<RK, RV>,
        maintain: bool,
    ) -> Vec<MapDiff<K, Self::Output>> {
        let preserve_batch = matches!(diff, MapDiff::Batch { .. });
        let head_changes = self.head.apply_right_diff(diff, maintain);
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

impl<Location, K, Input, RK, RV, Head, Tail> RightRoot<There<Location>, K, Input, RK, RV>
    for JCons<Head, Tail>
where
    K: Hash + Eq + CellValue,
    Input: CellValue,
    RK: Hash + Eq + CellValue,
    RV: CellValue,
    Head: ExecutableStage<Key = K, Input = Input>,
    Tail: RuntimeStages<K, Head::Output> + RightRoot<Location, K, Head::Output, RK, RV>,
{
    fn apply_right_root_diff_policy(
        &mut self,
        diff: &MapDiff<RK, RV>,
        maintain: bool,
    ) -> Vec<MapDiff<K, Self::Output>> {
        self.tail.apply_right_root_diff_policy(diff, maintain)
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

impl<Left, Stages, K, Input, Current> JoinRegion<Left, Stages, K, Input>
where
    K: Hash + Eq + CellValue,
    Input: CellValue,
    Stages: StageList<K, Input, Output = Current>,
    Current: CellValue,
{
    /// Fuse a key-preserving projection into the final join stage.
    pub fn map_values<Output, F>(
        self,
        map: F,
    ) -> JoinRegion<Left, <Stages as MapLast<F, Output>>::Stages, K, Input>
    where
        Output: CellValue,
        F: Fn(&K, &Current) -> Output + Send + Sync + 'static,
        Stages: MapLast<F, Output>,
    {
        JoinRegion::new(self.left, self.stages.map_last(map))
    }

    /// Project the indexed matches of the final join directly.
    pub fn map_joined_values<Output, F>(
        self,
        project: F,
    ) -> JoinRegion<Left, <Stages as ReplaceLastProject<F, Output>>::Stages, K, Input>
    where
        Output: CellValue,
        Stages: LastStage + ReplaceLastProject<F, Output>,
        F: Fn(
                &K,
                &<Stages as LastStage>::Input,
                &[(
                    <Stages as LastStage>::RightKey,
                    <Stages as LastStage>::RightValue,
                )],
            ) -> Output
            + Send
            + Sync
            + 'static,
    {
        JoinRegion::new(self.left, self.stages.replace_last_project(project))
    }

    /// Append a relationship-typed left join to this physical region.
    #[allow(clippy::type_complexity)]
    pub fn left_join_fk<Rel, R>(
        self,
        right: R,
    ) -> JoinRegion<
        Left,
        <Stages as Push<
            JoinStage<
                R,
                K,
                Current,
                R::Key,
                Rel::Child,
                K,
                (Current, Vec<Rel::Child>),
                fn(&K, &Current) -> K,
                OptionalRightKey<fn(&R::Key, &Rel::Child) -> Option<K>>,
                DirectProject<
                    fn(&K, &Current, &[(R::Key, Rel::Child)]) -> (Current, Vec<Rel::Child>),
                >,
                SharedRelationIndex<Rel>,
            >,
        >>::Output,
        K,
        Input,
    >
    where
        Rel: ForeignKeyRelation,
        R: MapQuery<Value = Rel::Child>,
        Rel::ForeignKey: IdFor<Rel::Parent, MapKey = K>,
        Stages: Push<
            JoinStage<
                R,
                K,
                Current,
                R::Key,
                Rel::Child,
                K,
                (Current, Vec<Rel::Child>),
                fn(&K, &Current) -> K,
                OptionalRightKey<fn(&R::Key, &Rel::Child) -> Option<K>>,
                DirectProject<
                    fn(&K, &Current, &[(R::Key, Rel::Child)]) -> (Current, Vec<Rel::Child>),
                >,
                SharedRelationIndex<Rel>,
            >,
        >,
    {
        let project: DirectProject<
            fn(&K, &Current, &[(R::Key, Rel::Child)]) -> (Current, Vec<Rel::Child>),
        > = DirectProject(collect_matches::<K, Current, R::Key, Rel::Child>);
        let left_key: fn(&K, &Current) -> K = map_key::<K, Current>;
        let right_key: fn(&R::Key, &Rel::Child) -> Option<K> = foreign_map_key::<Rel, R::Key, K>;
        let stage = JoinStage::new(right, left_key, OptionalRightKey(right_key), project)
            .with_index_policy(SharedRelationIndex::<Rel>::new());
        JoinRegion::new(self.left, self.stages.push(stage))
    }

    /// Append another ad-hoc left join to this physical region.
    #[allow(clippy::type_complexity)]
    pub fn left_join_by<R, RK, RV, JK, FL, FR>(
        self,
        right: R,
        left_key: FL,
        right_key: FR,
    ) -> JoinRegion<
        Left,
        <Stages as Push<
            JoinStage<
                R,
                K,
                Current,
                RK,
                RV,
                JK,
                (Current, Vec<RV>),
                FL,
                RequiredRightKey<FR>,
                DirectProject<fn(&K, &Current, &[(RK, RV)]) -> (Current, Vec<RV>)>,
            >,
        >>::Output,
        K,
        Input,
    >
    where
        R: MapQuery<Key = RK, Value = RV>,
        RK: Hash + Eq + CellValue,
        RV: CellValue,
        JK: Hash + Eq + CellValue,
        FL: Fn(&K, &Current) -> JK + Send + Sync + 'static,
        FR: Fn(&RK, &RV) -> JK + Send + Sync + 'static,
        Stages: Push<
            JoinStage<
                R,
                K,
                Current,
                RK,
                RV,
                JK,
                (Current, Vec<RV>),
                FL,
                RequiredRightKey<FR>,
                DirectProject<fn(&K, &Current, &[(RK, RV)]) -> (Current, Vec<RV>)>,
            >,
        >,
    {
        let project: DirectProject<fn(&K, &Current, &[(RK, RV)]) -> (Current, Vec<RV>)> =
            DirectProject(collect_matches::<K, Current, RK, RV>);
        let stage = JoinStage::new(right, left_key, RequiredRightKey(right_key), project);
        JoinRegion::new(self.left, self.stages.push(stage))
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

/// Consumes the declarative stages into an executable state spine and a
/// parallel spine of right plans. `Location` is the statically typed direct
/// entry point of the next right root into the completed runtime spine.
trait SplitStages<K, Input, Location>
where
    K: Hash + Eq + CellValue,
    Input: CellValue,
{
    type Runtime: RuntimeStages<K, Input>;
    type Rights;

    fn split(self, cx: &CompileContext) -> (Self::Runtime, Self::Rights);
}

struct RightPlan<Right, Tail, Location, RK, RV, JK, Policy, Binding> {
    right: Right,
    tail: Tail,
    binding: Option<Binding>,
    _types: PhantomData<fn() -> (Location, RK, RV, JK, Policy)>,
}

impl<K, Input, Location> SplitStages<K, Input, Location> for JNil
where
    K: Hash + Eq + CellValue,
    Input: CellValue,
{
    type Runtime = Self;
    type Rights = Self;

    fn split(self, _: &CompileContext) -> (Self::Runtime, Self::Rights) {
        (Self, Self)
    }
}

impl<Right, Tail, K, Input, RK, RV, JK, Output, LeftKey, RightKeyFn, Project, Policy, Location>
    SplitStages<K, Input, Location>
    for JCons<
        JoinStage<Right, K, Input, RK, RV, JK, Output, LeftKey, RightKeyFn, Project, Policy>,
        Tail,
    >
where
    K: Hash + Eq + CellValue,
    Input: CellValue,
    RK: Hash + Eq + CellValue,
    RV: CellValue,
    JK: Hash + Eq + CellValue,
    Output: CellValue,
    LeftKey: Fn(&K, &Input) -> JK + Send + Sync + 'static,
    RightKeyFn: RightJoinKey<RK, RV, JK>,
    Project: StageProject<K, Input, RK, RV, Output>,
    Right: MapQuery<Key = RK, Value = RV>,
    Policy: IndexPolicy<RK, RV, JK>,
    Tail: SplitStages<K, Output, There<Location>>,
{
    type Runtime = JCons<
        StageRuntimeState<
            K,
            Input,
            RK,
            RV,
            JK,
            Output,
            LeftKey,
            RightKeyFn,
            Project,
            Policy::Storage,
        >,
        Tail::Runtime,
    >;
    type Rights = RightPlan<Right, Tail::Rights, Location, RK, RV, JK, Policy, Policy::Binding>;

    fn split(self, cx: &CompileContext) -> (Self::Runtime, Self::Rights) {
        let JoinStage {
            right,
            left_key,
            right_key,
            project,
            index_policy: _,
            _types: _,
        } = self.head;
        let shareable = right.raw_source_identity().is_some();
        let (right_index, binding) = Policy::prepare(cx, shareable);
        let (tail_runtime, tail_rights) = self.tail.split(cx);
        (
            JCons {
                head: StageRuntimeState::with_index(left_key, right_key, project, right_index),
                tail: tail_runtime,
            },
            RightPlan {
                right,
                tail: tail_rights,
                binding,
                _types: PhantomData,
            },
        )
    }
}

/// Installs right roots in stage order. Each callback enters the shared state
/// at its own `Here`/`There` location and emits only changes at the end of the
/// complete runtime spine.
trait InstallRights<Runtime, K, Input, Output>
where
    K: Hash + Eq + CellValue,
    Input: CellValue,
    Output: CellValue,
    Runtime: RuntimeStages<K, Input, Output = Output>,
{
    fn install<Sink>(
        self,
        cx: &mut CompileContext,
        state: &Arc<Mutex<RegionRouter<Runtime, K, Input>>>,
        sink: &Arc<Sink>,
    ) -> Vec<SubscriptionGuard>
    where
        Sink: MapDiffSink<K, Output>;
}

impl<Runtime, K, Input, Output> InstallRights<Runtime, K, Input, Output> for JNil
where
    K: Hash + Eq + CellValue,
    Input: CellValue,
    Output: CellValue,
    Runtime: RuntimeStages<K, Input, Output = Output>,
{
    fn install<Sink>(
        self,
        _cx: &mut CompileContext,
        _state: &Arc<Mutex<RegionRouter<Runtime, K, Input>>>,
        _sink: &Arc<Sink>,
    ) -> Vec<SubscriptionGuard>
    where
        Sink: MapDiffSink<K, Output>,
    {
        Vec::new()
    }
}

impl<Runtime, K, Input, Output, Right, Tail, Location, RK, RV, JK, Policy, Binding>
    InstallRights<Runtime, K, Input, Output>
    for RightPlan<Right, Tail, Location, RK, RV, JK, Policy, Binding>
where
    K: Hash + Eq + CellValue,
    Input: CellValue,
    Output: CellValue,
    RK: Hash + Eq + CellValue,
    RV: CellValue,
    JK: Hash + Eq + CellValue,
    Policy: IndexPolicy<RK, RV, JK, Binding = Binding>,
    Binding: Clone + Send + Sync + 'static,
    Runtime: RuntimeStages<K, Input, Output = Output>
        + RightRoot<Location, K, Input, RK, RV>
        + EmptyShardRuntime
        + HeadInputSnapshot<K, Input>
        + RuntimeStageCost
        + Send
        + 'static,
    Right: MapQuery<Key = RK, Value = RV>,
    Tail: InstallRights<Runtime, K, Input, Output>,
{
    fn install<Sink>(
        self,
        cx: &mut CompileContext,
        state: &Arc<Mutex<RegionRouter<Runtime, K, Input>>>,
        sink: &Arc<Sink>,
    ) -> Vec<SubscriptionGuard>
    where
        Sink: MapDiffSink<K, Output>,
    {
        let callback_binding = self.binding.clone();
        let right_state = Arc::clone(state);
        let right_sink = Arc::clone(sink);
        let callback = move |diff: &MapDiff<RK, RV>| {
            let maintain = Policy::maintains(callback_binding.as_ref());
            let changes = region_transaction(&right_state, |runtime| {
                runtime.apply_right::<Location, RK, RV>(diff, maintain)
            });
            for change in &changes {
                right_sink(change);
            }
        };
        let mut guards = Policy::install(self.right, cx, self.binding, callback);
        guards.extend(self.tail.install(cx, state, sink));
        guards
    }
}

impl<Left, Stages, K, Input> BuildQueryRuntime<K, Stages::Output>
    for JoinRegion<Left, Stages, K, Input>
where
    K: Hash + Eq + CellValue,
    Input: CellValue,
    Left: MapQuery<Key = K, Value = Input> + PlanProperties<OutputPartition = ByMapKey<K>>,
    Stages: StageList<K, Input> + SplitStages<K, Input, Here> + Send + Sync + 'static,
    Stages::Output: CellValue,
    Stages::Runtime: RuntimeStages<K, Input, Output = Stages::Output>
        + EmptyShardRuntime
        + HeadInputSnapshot<K, Input>
        + RuntimeStageCost
        + Send,
    Stages::Rights: InstallRights<Stages::Runtime, K, Input, Stages::Output>,
{
    fn build_into<Sink>(self, cx: &mut CompileContext, sink: Sink) -> Vec<SubscriptionGuard>
    where
        Sink: MapDiffSink<K, Stages::Output>,
    {
        let (runtime, rights) = self.stages.split(cx);
        let query_poison = cx.query_poison();
        #[cfg(test)]
        let router = if let Some(config) = cx.test_region_config() {
            RegionRouter::with_test_config(runtime, query_poison, config)
        } else {
            RegionRouter::with_query_poison(runtime, query_poison)
        };
        #[cfg(not(test))]
        let router = RegionRouter::with_query_poison(runtime, query_poison);
        let state = Arc::new(Mutex::new(router));
        let sink = Arc::new(sink);

        // Register every right root before the left root. CompileContext then
        // activates their initial snapshots in this same order, so the first
        // left snapshot observes all right indexes fully populated.
        let mut guards = rights.install(cx, &state, &sink);
        let left_state = state;
        let left_sink = sink;
        guards.extend(compile_runtime_into(
            self.left,
            cx,
            move |diff: &MapDiff<K, Input>| {
                let changes = region_transaction(&left_state, |runtime| runtime.apply_left(diff));
                for change in &changes {
                    left_sink(change);
                }
            },
        ));
        guards
    }
}

#[allow(private_bounds)]
impl<Left, Stages, K, Input> MapQuery for JoinRegion<Left, Stages, K, Input>
where
    K: Hash + Eq + CellValue,
    Input: CellValue,
    Left: MapQuery<Key = K, Value = Input> + PlanProperties<OutputPartition = ByMapKey<K>>,
    Stages: StageList<K, Input> + SplitStages<K, Input, Here> + Send + Sync + 'static,
    Stages::Output: CellValue,
    Stages::Runtime: RuntimeStages<K, Input, Output = Stages::Output>
        + EmptyShardRuntime
        + HeadInputSnapshot<K, Input>
        + RuntimeStageCost
        + Send,
    Stages::Rights: InstallRights<Stages::Runtime, K, Input, Stages::Output>,
{
    type Key = K;
    type Value = Stages::Output;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        map_query::properties::ExactlyOne, pipeline::Materialize as _,
        traits::watchable::Gettable as _,
    };

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
        assert!(!runtime.right.rows.contains_key(&7));

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

    #[test]
    #[allow(clippy::too_many_lines)]
    fn heterogeneous_three_stage_runtime_propagates_every_root_directly() {
        let stage1 = StageRuntimeState::new(
            |_: &u32, _: &i8| 0_u8,
            crate::traits::RequiredRightKey(|_: &u8, _: &i8| 0_u8),
            DirectProject(|_: &u32, left: &i8, rights: &[(u8, i8)]| {
                i16::from(*left)
                    + rights
                        .iter()
                        .map(|(_, value)| i16::from(*value))
                        .sum::<i16>()
            }),
        );
        let stage2 = StageRuntimeState::new(
            |_: &u32, _: &i16| 0_u8,
            crate::traits::RequiredRightKey(|_: &u16, _: &i16| 0_u8),
            DirectProject(|_: &u32, left: &i16, rights: &[(u16, i16)]| {
                i32::from(*left)
                    + rights
                        .iter()
                        .map(|(_, value)| i32::from(*value))
                        .sum::<i32>()
            }),
        );
        let stage3 = StageRuntimeState::new(
            |_: &u32, _: &i32| 0_u8,
            crate::traits::RequiredRightKey(|_: &u32, _: &i32| 0_u8),
            FilterProject::new(
                DirectProject(|_: &u32, left: &i32, rights: &[(u32, i32)]| {
                    i64::from(*left)
                        + rights
                            .iter()
                            .map(|(_, value)| i64::from(*value))
                            .sum::<i64>()
                }),
                |_: &u32, output: &i64| *output >= 0,
            ),
        );
        let mut runtime = JCons {
            head: stage1,
            tail: JCons {
                head: stage2,
                tail: JCons {
                    head: stage3,
                    tail: JNil,
                },
            },
        };

        assert_eq!(
            runtime.apply_left_diff(&MapDiff::Insert { key: 1, value: 10 }),
            vec![MapDiff::Insert { key: 1, value: 10 }]
        );
        assert_eq!(
            <_ as RightRoot<Here, u32, i8, u8, i8>>::apply_right_root_diff(
                &mut runtime,
                &MapDiff::Insert { key: 1, value: 5 },
            ),
            vec![MapDiff::Update {
                key: 1,
                old_value: 10,
                new_value: 15
            }]
        );
        assert_eq!(
            <_ as RightRoot<Here, u32, i8, u8, i8>>::apply_right_root_diff(
                &mut runtime,
                &MapDiff::Update {
                    key: 1,
                    old_value: 5,
                    new_value: 6
                },
            ),
            vec![MapDiff::Update {
                key: 1,
                old_value: 15,
                new_value: 16
            }]
        );
        assert_eq!(
            <_ as RightRoot<There<Here>, u32, i8, u16, i16>>::apply_right_root_diff(
                &mut runtime,
                &MapDiff::Insert { key: 2, value: 7 },
            ),
            vec![MapDiff::Update {
                key: 1,
                old_value: 16,
                new_value: 23
            }]
        );
        assert_eq!(
            <_ as RightRoot<There<Here>, u32, i8, u16, i16>>::apply_right_root_diff(
                &mut runtime,
                &MapDiff::Update {
                    key: 2,
                    old_value: 7,
                    new_value: 8
                },
            ),
            vec![MapDiff::Update {
                key: 1,
                old_value: 23,
                new_value: 24
            }]
        );
        assert_eq!(
            <_ as RightRoot<There<There<Here>>, u32, i8, u32, i32>>::apply_right_root_diff(
                &mut runtime,
                &MapDiff::Insert { key: 3, value: 11 },
            ),
            vec![MapDiff::Update {
                key: 1,
                old_value: 24,
                new_value: 35
            }]
        );

        assert_eq!(
            <_ as RightRoot<There<Here>, u32, i8, u16, i16>>::apply_right_root_diff(
                &mut runtime,
                &MapDiff::Remove {
                    key: 2,
                    old_value: 8
                },
            ),
            vec![MapDiff::Update {
                key: 1,
                old_value: 35,
                new_value: 27
            }]
        );
        assert_eq!(
            <_ as RightRoot<Here, u32, i8, u8, i8>>::apply_right_root_diff(
                &mut runtime,
                &MapDiff::Remove {
                    key: 1,
                    old_value: 6
                },
            ),
            vec![MapDiff::Update {
                key: 1,
                old_value: 27,
                new_value: 21
            }]
        );
        assert_eq!(
            <_ as RightRoot<There<There<Here>>, u32, i8, u32, i32>>::apply_right_root_diff(
                &mut runtime,
                &MapDiff::Update {
                    key: 3,
                    old_value: 11,
                    new_value: -20
                },
            ),
            vec![MapDiff::Remove {
                key: 1,
                old_value: 21
            }]
        );
        assert_eq!(
            <_ as RightRoot<There<There<Here>>, u32, i8, u32, i32>>::apply_right_root_diff(
                &mut runtime,
                &MapDiff::Remove {
                    key: 3,
                    old_value: -20
                },
            ),
            vec![MapDiff::Insert { key: 1, value: 10 }]
        );
        assert_eq!(
            runtime.apply_left_diff(&MapDiff::Update {
                key: 1,
                old_value: 10,
                new_value: 12
            }),
            vec![MapDiff::Update {
                key: 1,
                old_value: 10,
                new_value: 12
            }]
        );
        assert_eq!(
            runtime.apply_left_diff(&MapDiff::Remove {
                key: 1,
                old_value: 12
            }),
            vec![MapDiff::Remove {
                key: 1,
                old_value: 12
            }]
        );
    }

    #[test]
    fn stage_batches_preserve_each_member_event() {
        let mut runtime = StageRuntimeState::new(
            |_: &u8, value: &i32| *value,
            crate::traits::RequiredRightKey(|_: &u8, value: &i32| *value),
            DirectProject(|_: &u8, left: &i32, _: &[(u8, i32)]| *left),
        );
        assert_eq!(
            runtime.apply_left_diff(&MapDiff::Batch {
                changes: vec![
                    MapDiff::Insert { key: 1, value: 10 },
                    MapDiff::Update {
                        key: 1,
                        old_value: 10,
                        new_value: 20
                    },
                    MapDiff::Remove {
                        key: 1,
                        old_value: 20
                    },
                ],
            }),
            vec![
                MapDiff::Insert { key: 1, value: 10 },
                MapDiff::Update {
                    key: 1,
                    old_value: 10,
                    new_value: 20
                },
                MapDiff::Remove {
                    key: 1,
                    old_value: 20
                },
            ]
        );

        let mut right_runtime = StageRuntimeState::new(
            |_: &u8, _: &i32| 0_u8,
            crate::traits::RequiredRightKey(|_: &u8, _: &i32| 0_u8),
            DirectProject(|_: &u8, left: &i32, rights: &[(u8, i32)]| {
                *left + rights.iter().map(|(_, value)| value).sum::<i32>()
            }),
        );
        assert_eq!(
            right_runtime.apply_left_diff(&MapDiff::Insert { key: 1, value: 10 }),
            vec![MapDiff::Insert { key: 1, value: 10 }]
        );
        assert_eq!(
            right_runtime.apply_right_diff(&MapDiff::Batch {
                changes: vec![
                    MapDiff::Insert { key: 2, value: 1 },
                    MapDiff::Update {
                        key: 2,
                        old_value: 1,
                        new_value: 2
                    },
                    MapDiff::Remove {
                        key: 2,
                        old_value: 2
                    },
                ],
            }),
            vec![
                MapDiff::Update {
                    key: 1,
                    old_value: 10,
                    new_value: 11
                },
                MapDiff::Update {
                    key: 1,
                    old_value: 11,
                    new_value: 12
                },
                MapDiff::Update {
                    key: 1,
                    old_value: 12,
                    new_value: 10
                },
            ]
        );
    }

    #[test]
    fn heterogeneous_eight_stage_runtime_smoke() {
        macro_rules! stage {
            () => {
                StageRuntimeState::new(
                    |_: &u32, _: &i32| 0_u8,
                    crate::traits::RequiredRightKey(|_: &u8, _: &u8| 0_u8),
                    DirectProject(|_: &u32, left: &i32, _: &[(u8, u8)]| *left),
                )
            };
        }
        let mut runtime = JNil
            .push(stage!())
            .push(stage!())
            .push(stage!())
            .push(stage!())
            .push(stage!())
            .push(stage!())
            .push(stage!())
            .push(stage!());
        assert_eq!(
            runtime.apply_left_diff(&MapDiff::Insert { key: 1, value: 42 }),
            vec![MapDiff::Insert { key: 1, value: 42 }]
        );
    }
    #[test]
    #[allow(clippy::too_many_lines)]
    fn explicit_three_stage_region_materializes_and_tears_down_all_roots() {
        use crate::traits::{DepNode, RequiredRightKey};

        let left = crate::CellMap::<u32, u32>::new();
        let right1 = crate::CellMap::<u8, (u32, i32)>::new();
        let right2 = crate::CellMap::<u16, (u32, &'static str)>::new();
        let right3 = crate::CellMap::<u64, (u32, bool)>::new();

        right1.insert(1, (7, 10));
        right2.insert(3, (7, "a"));
        right3.insert(5, (7, true));
        left.insert(100, 7);

        let stages = JNil
            .push(
                JoinStage::<_, u32, u32, u8, (u32, i32), u32, _, _, _, _>::new(
                    right1.clone(),
                    |_: &u32, value: &u32| *value,
                    RequiredRightKey(|_: &u8, value: &(u32, i32)| value.0),
                    DirectProject(|_: &u32, left: &u32, rights: &[(u8, (u32, i32))]| {
                        (
                            *left,
                            rights.iter().map(|(_, value)| value.1).collect::<Vec<_>>(),
                        )
                    }),
                ),
            )
            .push(JoinStage::<
                _,
                u32,
                (u32, Vec<i32>),
                u16,
                (u32, &'static str),
                u32,
                _,
                _,
                _,
                _,
            >::new(
                right2.clone(),
                |_: &u32, value: &(u32, Vec<i32>)| value.0,
                RequiredRightKey(|_: &u16, value: &(u32, &'static str)| value.0),
                DirectProject(
                    |_: &u32, left: &(u32, Vec<i32>), rights: &[(u16, (u32, &'static str))]| {
                        (
                            left.0,
                            left.1.clone(),
                            rights.iter().map(|(_, value)| value.1).collect::<Vec<_>>(),
                        )
                    },
                ),
            ))
            .push(JoinStage::<
                _,
                u32,
                (u32, Vec<i32>, Vec<&'static str>),
                u64,
                (u32, bool),
                u32,
                _,
                _,
                _,
                _,
            >::new(
                right3.clone(),
                |_: &u32, value: &(u32, Vec<i32>, Vec<&'static str>)| value.0,
                RequiredRightKey(|_: &u64, value: &(u32, bool)| value.0),
                DirectProject(
                    |_: &u32,
                     left: &(u32, Vec<i32>, Vec<&'static str>),
                     rights: &[(u64, (u32, bool))]| {
                        (
                            left.0,
                            left.1.clone(),
                            left.2.clone(),
                            rights.iter().map(|(_, value)| value.1).collect::<Vec<_>>(),
                        )
                    },
                ),
            ));

        let output = JoinRegion::new(left.clone(), stages).materialize();
        assert_eq!(left.inner.diffs_cell.subscriber_count(), 1);
        assert_eq!(right1.inner.diffs_cell.subscriber_count(), 1);
        assert_eq!(right2.inner.diffs_cell.subscriber_count(), 1);
        assert_eq!(right3.inner.diffs_cell.subscriber_count(), 1);
        assert_eq!(
            output.get_value(&100),
            Some((7, vec![10], vec!["a"], vec![true]))
        );

        // Exercise every right root and the left root after installation.
        right1.insert(2, (7, 20));
        right1.insert(1, (7, 11));
        right2.insert(4, (7, "b"));
        right2.remove(&3);
        right3.insert(6, (7, false));
        assert_eq!(
            output.get_value(&100),
            Some((7, vec![11, 20], vec!["b"], vec![true, false]))
        );
        left.insert(100, 8);
        right1.insert(8, (8, 80));
        right2.insert(8, (8, "eight"));
        right3.insert(8, (8, false));
        assert_eq!(
            output.get_value(&100),
            Some((8, vec![80], vec!["eight"], vec![false]))
        );

        drop(output);
        assert_eq!(left.inner.diffs_cell.subscriber_count(), 0);
        assert_eq!(right1.inner.diffs_cell.subscriber_count(), 0);
        assert_eq!(right2.inner.diffs_cell.subscriber_count(), 0);
        assert_eq!(right3.inner.diffs_cell.subscriber_count(), 0);
    }

    #[test]
    #[cfg(not(target_arch = "wasm32"))]
    fn forced_region_reentrant_different_root_settles_fifo_before_return() {
        use crate::map_query::compiler::{TestRegionConfig, TestRegionDispatch};
        use crate::traits::{DepNode, RequiredRightKey};
        use std::sync::atomic::{AtomicBool, Ordering};

        let left = crate::CellMap::<u32, u32>::new();
        let right = crate::CellMap::<u8, (u32, i32)>::new();
        let left_baseline = left.inner.diffs_cell.subscriber_count();
        let right_baseline = right.inner.diffs_cell.subscriber_count();
        let stages = JNil.push(JoinStage::new(
            right.clone(),
            |_: &u32, value: &u32| *value,
            RequiredRightKey(|_: &u8, value: &(u32, i32)| value.0),
            DirectProject(|_: &u32, left: &u32, rights: &[(u8, (u32, i32))]| {
                (
                    *left,
                    rights.iter().map(|(_, value)| value.1).collect::<Vec<_>>(),
                )
            }),
        ));
        let mut cx = CompileContext::default();
        cx.set_test_region_config(TestRegionConfig {
            shards: 3,
            promote_after: 1,
            dispatch: TestRegionDispatch::ForceSerial,
        });
        let observed = Arc::new(Mutex::new(Vec::new()));
        let armed = Arc::new(AtomicBool::new(false));
        let fired = Arc::new(AtomicBool::new(false));
        let sink_observed = Arc::clone(&observed);
        let sink_armed = Arc::clone(&armed);
        let sink_fired = Arc::clone(&fired);
        let sink_right = right.clone();
        let mut guards = JoinRegion::new(left.clone(), stages).build_into(
            &mut cx,
            move |diff: &MapDiff<u32, (u32, Vec<i32>)>| {
                sink_observed
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(diff.clone());
                if sink_armed.load(Ordering::Acquire) && !sink_fired.swap(true, Ordering::AcqRel) {
                    sink_right.insert(1, (7, 5));
                }
            },
        );
        guards.extend(cx.activate());
        observed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        armed.store(true, Ordering::Release);

        left.insert(1, 7);

        assert_eq!(
            *observed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            vec![
                MapDiff::Insert {
                    key: 1,
                    value: (7, Vec::new()),
                },
                MapDiff::Update {
                    key: 1,
                    old_value: (7, Vec::new()),
                    new_value: (7, vec![5]),
                },
            ]
        );
        assert!(fired.load(Ordering::Acquire));
        drop(guards);
        assert_eq!(left.inner.diffs_cell.subscriber_count(), left_baseline);
        assert_eq!(right.inner.diffs_cell.subscriber_count(), right_baseline);
    }

    #[test]
    #[cfg(not(target_arch = "wasm32"))]
    #[allow(clippy::too_many_lines)]
    fn forced_region_concurrent_right_root_queues_and_settles_before_active_return() {
        use crate::map_query::compiler::{TestRegionConfig, TestRegionDispatch};
        use crate::traits::{DepNode, RequiredRightKey};
        use std::sync::atomic::{AtomicBool, Ordering};

        let left = crate::CellMap::<u32, u32>::new();
        let right = crate::CellMap::<u8, (u32, i32)>::new();
        let left_baseline = left.inner.diffs_cell.subscriber_count();
        let right_baseline = right.inner.diffs_cell.subscriber_count();
        let armed = Arc::new(AtomicBool::new(false));
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let release_rx = Arc::new(Mutex::new(release_rx));
        let projection_armed = Arc::clone(&armed);
        let projection_release = Arc::clone(&release_rx);
        let stages = JNil.push(JoinStage::new(
            right.clone(),
            |_: &u32, value: &u32| *value,
            RequiredRightKey(|_: &u8, value: &(u32, i32)| value.0),
            DirectProject(move |_: &u32, left: &u32, rights: &[(u8, (u32, i32))]| {
                if projection_armed.swap(false, Ordering::AcqRel) {
                    assert!(entered_tx.send(()).is_ok());
                    assert!(
                        projection_release
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .recv()
                            .is_ok()
                    );
                }
                (
                    *left,
                    rights.iter().map(|(_, value)| value.1).collect::<Vec<_>>(),
                )
            }),
        ));
        let mut cx = CompileContext::default();
        cx.set_test_region_config(TestRegionConfig {
            shards: 3,
            promote_after: 1,
            dispatch: TestRegionDispatch::ForceSerial,
        });
        let observed = Arc::new(Mutex::new(Vec::new()));
        let sink_observed = Arc::clone(&observed);
        let mut guards = JoinRegion::new(left.clone(), stages).build_into(
            &mut cx,
            move |diff: &MapDiff<u32, (u32, Vec<i32>)>| {
                sink_observed
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(diff.clone());
            },
        );
        guards.extend(cx.activate());
        observed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        armed.store(true, Ordering::Release);

        let active_left = left.clone();
        let active = std::thread::spawn(move || active_left.insert(1, 7));
        assert!(entered_rx.recv().is_ok(), "left projection must be active");
        let concurrent_right = right.clone();
        let admitted = std::thread::spawn(move || concurrent_right.insert(1, (7, 5)));
        assert!(admitted.join().is_ok(), "right event admission must return");
        assert!(
            observed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty(),
            "the blocked left callback cannot publish a partial result"
        );
        assert!(release_tx.send(()).is_ok());
        assert!(
            active.join().is_ok(),
            "active source call must settle queued work"
        );

        let trace = observed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        assert_eq!(
            trace,
            vec![
                MapDiff::Insert {
                    key: 1,
                    value: (7, Vec::new()),
                },
                MapDiff::Update {
                    key: 1,
                    old_value: (7, Vec::new()),
                    new_value: (7, vec![5]),
                },
            ]
        );
        let mut final_state = std::collections::BTreeMap::new();
        apply_trace(&mut final_state, &trace);
        assert_eq!(
            final_state,
            std::collections::BTreeMap::from([(1, (7, vec![5]))])
        );
        drop(guards);
        assert_eq!(left.inner.diffs_cell.subscriber_count(), left_baseline);
        assert_eq!(right.inner.diffs_cell.subscriber_count(), right_baseline);
    }

    #[test]
    #[allow(clippy::trivially_copy_pass_by_ref)]
    fn explicit_eight_stage_region_materialization_smoke() {
        use crate::traits::RequiredRightKey;

        type Right = crate::CellMap<u8, (u32, u32)>;
        type Stage = JoinStage<
            Right,
            u32,
            u32,
            u8,
            (u32, u32),
            u32,
            u32,
            fn(&u32, &u32) -> u32,
            RequiredRightKey<fn(&u8, &(u32, u32)) -> u32>,
            DirectProject<fn(&u32, &u32, &[(u8, (u32, u32))]) -> u32>,
        >;

        fn left_key(_: &u32, _: &u32) -> u32 {
            0
        }
        fn right_key(_: &u8, right: &(u32, u32)) -> u32 {
            right.0
        }
        fn project(_: &u32, left: &u32, rights: &[(u8, (u32, u32))]) -> u32 {
            left + rights.iter().map(|(_, right)| right.1).sum::<u32>()
        }
        fn stage(right: Right) -> Stage {
            Stage::new(
                right,
                left_key,
                RequiredRightKey(right_key),
                DirectProject(project),
            )
        }
        fn right() -> Right {
            let right = Right::new();
            right.insert(1, (0, 1));
            right
        }

        let left = crate::CellMap::<u32, u32>::new();
        left.insert(1, 0);
        let stages = JNil
            .push(stage(right()))
            .push(stage(right()))
            .push(stage(right()))
            .push(stage(right()))
            .push(stage(right()))
            .push(stage(right()))
            .push(stage(right()))
            .push(stage(right()));
        let output = JoinRegion::new(left, stages).materialize();
        assert_eq!(output.get_value(&1), Some(8));
    }

    struct SharedTestRelation;

    type SharedRight = crate::CellMap<u32, (u32, u32)>;
    type SharedStage = JoinStage<
        SharedRight,
        u32,
        u32,
        u32,
        (u32, u32),
        u32,
        u32,
        fn(&u32, &u32) -> u32,
        RequiredRightKey<fn(&u32, &(u32, u32)) -> u32>,
        DirectProject<fn(&u32, &u32, &[(u32, (u32, u32))]) -> u32>,
        SharedRelationIndex<SharedTestRelation>,
    >;

    #[allow(
        clippy::arithmetic_side_effects,
        clippy::as_conversions,
        clippy::trivially_copy_pass_by_ref
    )]
    fn shared_stage(right: SharedRight) -> SharedStage {
        fn left_key(_: &u32, _: &u32) -> u32 {
            7
        }
        fn right_key(_: &u32, value: &(u32, u32)) -> u32 {
            value.0
        }
        fn project(_: &u32, left: &u32, rights: &[(u32, (u32, u32))]) -> u32 {
            left + rights.iter().map(|(_, value)| value.1).sum::<u32>()
        }
        JoinStage::new(
            right,
            left_key as fn(&u32, &u32) -> u32,
            RequiredRightKey(right_key as fn(&u32, &(u32, u32)) -> u32),
            DirectProject(project as fn(&u32, &u32, &[(u32, (u32, u32))]) -> u32),
        )
        .with_index_policy(SharedRelationIndex::new())
    }

    #[test]
    fn arbitrary_three_stage_repeated_raw_relation_shares_one_physical_index() {
        let left = crate::CellMap::<u32, u32>::new();
        let right = SharedRight::new();
        left.insert(1, 1);
        right.insert(1, (7, 10));

        let stages = JNil
            .push(shared_stage(right.clone()))
            .push(shared_stage(right.clone()))
            .push(shared_stage(right.clone()));
        let mut cx = CompileContext::default();
        let guards = JoinRegion::new(left.clone(), stages).build_into(&mut cx, |_| {});
        assert_eq!(cx.physical_relationship_count(), 1);
        let identity = BuildQueryRuntime::raw_source_identity(&right);
        assert!(identity.is_some());
        let Some(identity) = identity else { return };
        assert_eq!(cx.relationship_use_count::<SharedTestRelation>(identity), 3);
        drop(guards);

        let stages = JNil
            .push(shared_stage(right.clone()))
            .push(shared_stage(right.clone()))
            .push(shared_stage(right.clone()));
        let output = JoinRegion::new(left, stages).materialize();
        assert_eq!(output.get_value(&1), Some(31));
        right.insert(1, (7, 20));
        assert_eq!(output.get_value(&1), Some(61));
        right.insert(2, (7, 5));
        assert_eq!(output.get_value(&1), Some(76));
    }

    #[test]
    #[allow(clippy::arithmetic_side_effects, clippy::as_conversions)]
    fn transformed_relation_marked_rights_keep_private_indexes() {
        use crate::traits::MapValuesExt;

        let left = crate::CellMap::<u32, u32>::new();
        let right = SharedRight::new();
        let first = right.clone().map_values(|_, value| *value);
        let second = right.map_values(|_, value| (value.0, value.1 * 2));
        let first = JoinStage::new(
            first,
            (|_: &u32, _: &u32| 7) as fn(&u32, &u32) -> u32,
            RequiredRightKey(
                (|_: &u32, value: &(u32, u32)| value.0) as fn(&u32, &(u32, u32)) -> u32,
            ),
            DirectProject(
                (|_: &u32, left: &u32, rights: &[(u32, (u32, u32))]| {
                    left + rights.iter().map(|(_, v)| v.1).sum::<u32>()
                }) as fn(&u32, &u32, &[(u32, (u32, u32))]) -> u32,
            ),
        )
        .with_index_policy(SharedRelationIndex::<SharedTestRelation>::new());
        let second = JoinStage::new(
            second,
            (|_: &u32, _: &u32| 7) as fn(&u32, &u32) -> u32,
            RequiredRightKey(
                (|_: &u32, value: &(u32, u32)| value.0) as fn(&u32, &(u32, u32)) -> u32,
            ),
            DirectProject(
                (|_: &u32, left: &u32, rights: &[(u32, (u32, u32))]| {
                    left + rights.iter().map(|(_, v)| v.1).sum::<u32>()
                }) as fn(&u32, &u32, &[(u32, (u32, u32))]) -> u32,
            ),
        )
        .with_index_policy(SharedRelationIndex::<SharedTestRelation>::new());
        let mut cx = CompileContext::default();
        let guards =
            JoinRegion::new(left, JNil.push(first).push(second)).build_into(&mut cx, |_| {});
        assert_eq!(cx.physical_relationship_count(), 0);
        drop(guards);
    }

    #[test]
    fn region_router_promotes_three_stage_spine_deterministically_for_three_and_eight_shards() {
        let make_runtime = || {
            let first = StageRuntimeState::with_index(
                |_: &u32, value: &i32| *value % 3,
                crate::traits::RequiredRightKey(|_: &u8, value: &i32| *value % 3),
                DirectProject(|_: &u32, left: &i32, rights: &[(u8, i32)]| {
                    *left + rights.iter().map(|(_, value)| *value).sum::<i32>()
                }),
                DeferredPhysical::default(),
            );
            let second = StageRuntimeState::with_index(
                |_: &u32, value: &i32| *value % 5,
                crate::traits::RequiredRightKey(|_: &u16, value: &i32| *value % 5),
                DirectProject(|_: &u32, left: &i32, rights: &[(u16, i32)]| {
                    *left + rights.iter().map(|(_, value)| *value).sum::<i32>()
                }),
                DeferredPhysical::default(),
            );
            let third = StageRuntimeState::with_index(
                |_: &u32, value: &i32| *value % 7,
                crate::traits::RequiredRightKey(|_: &u32, value: &i32| *value % 7),
                DirectProject(|_: &u32, left: &i32, rights: &[(u32, i32)]| {
                    *left + rights.iter().map(|(_, value)| *value).sum::<i32>()
                }),
                DeferredPhysical::default(),
            );
            JCons {
                head: first,
                tail: JCons {
                    head: second,
                    tail: JCons {
                        head: third,
                        tail: JNil,
                    },
                },
            }
        };

        for shard_count in [3, 8] {
            let initial = MapDiff::Initial {
                entries: vec![(9, 9), (2, 2), (7, 7), (1, 1)],
            };
            let mut sequential = RegionRouter::with_config(make_runtime(), 1, 1);
            let expected = sequential.apply_left(&initial);
            let mut router = RegionRouter::with_config(make_runtime(), shard_count, 1);
            assert_eq!(router.apply_left(&initial), expected);

            let right = MapDiff::Insert {
                key: 40_u16,
                value: 5_i32,
            };
            let expected = sequential.apply_right::<There<Here>, _, _>(&right, true);
            assert_eq!(
                router.apply_right::<There<Here>, _, _>(&right, true),
                expected
            );

            let first_right = MapDiff::Batch {
                changes: vec![
                    MapDiff::Insert {
                        key: 3_u8,
                        value: 3_i32,
                    },
                    MapDiff::Update {
                        key: 3,
                        old_value: 3,
                        new_value: 6,
                    },
                ],
            };
            let expected = sequential.apply_right::<Here, _, _>(&first_right, true);
            assert_eq!(
                router.apply_right::<Here, _, _>(&first_right, true),
                expected
            );

            let distant_right = MapDiff::Insert {
                key: 7_u32,
                value: 7_i32,
            };
            let expected = sequential.apply_right::<There<There<Here>>, _, _>(&distant_right, true);
            assert_eq!(
                router.apply_right::<There<There<Here>>, _, _>(&distant_right, true),
                expected
            );
        }
    }

    #[test]
    fn region_router_promotes_from_live_batch_without_intermediate_outputs() {
        let make_runtime = || JCons {
            head: StageRuntimeState::with_index(
                |_: &u32, value: &i32| *value % 2,
                crate::traits::RequiredRightKey(|_: &u8, value: &i32| *value % 2),
                DirectProject(|_: &u32, left: &i32, _: &[(u8, i32)]| *left),
                DeferredPhysical::default(),
            ),
            tail: JCons {
                head: StageRuntimeState::with_index(
                    |_: &u32, value: &i32| *value % 3,
                    crate::traits::RequiredRightKey(|_: &u16, value: &i32| *value % 3),
                    DirectProject(|_: &u32, left: &i32, _: &[(u16, i32)]| *left),
                    DeferredPhysical::default(),
                ),
                tail: JCons {
                    head: StageRuntimeState::with_index(
                        |_: &u32, value: &i32| *value % 5,
                        crate::traits::RequiredRightKey(|_: &u32, value: &i32| *value % 5),
                        DirectProject(|_: &u32, left: &i32, _: &[(u32, i32)]| *left),
                        DeferredPhysical::default(),
                    ),
                    tail: JNil,
                },
            },
        };
        let first = MapDiff::Insert { key: 10, value: 10 };
        let batch = MapDiff::Batch {
            changes: vec![
                MapDiff::Update {
                    key: 10,
                    old_value: 10,
                    new_value: 11,
                },
                MapDiff::Insert { key: 3, value: 3 },
                MapDiff::Update {
                    key: 10,
                    old_value: 11,
                    new_value: 12,
                },
                MapDiff::Update {
                    key: 3,
                    old_value: 3,
                    new_value: 4,
                },
                MapDiff::Batch {
                    changes: vec![MapDiff::Insert { key: 8, value: 8 }],
                },
            ],
        };
        for shard_count in [3, 8] {
            let mut sequential = RegionRouter::with_config(make_runtime(), 1, 1);
            let mut router = RegionRouter::with_config(make_runtime(), shard_count, 2);
            assert_eq!(router.apply_left(&first), sequential.apply_left(&first));
            let expected = sequential.apply_left(&batch);
            let actual = router.apply_left(&batch);
            assert_eq!(actual, expected);
            assert!(matches!(actual.as_slice(), [MapDiff::Batch { .. }]));
            let replacement = MapDiff::Initial {
                entries: vec![(8, 80), (10, 110), (99, 99)],
            };
            assert_eq!(
                router.apply_left(&replacement),
                sequential.apply_left(&replacement)
            );
            assert!(router.shards.is_some());
        }
    }

    #[test]
    fn region_router_optional_right_rekeys_after_promotion() {
        let make_runtime = || JCons {
            head: StageRuntimeState::with_index(
                |_: &u32, value: &i32| *value % 3,
                crate::traits::OptionalRightKey(|_: &u8, value: &(Option<i32>, i32)| value.0),
                DirectProject(|_: &u32, left: &i32, rights: &[(u8, (Option<i32>, i32))]| {
                    *left + rights.iter().map(|(_, value)| value.1).sum::<i32>()
                }),
                DeferredPhysical::default(),
            ),
            tail: JCons {
                head: StageRuntimeState::with_index(
                    |_: &u32, value: &i32| *value % 5,
                    crate::traits::RequiredRightKey(|_: &u16, value: &i32| *value % 5),
                    DirectProject(|_: &u32, left: &i32, _: &[(u16, i32)]| *left),
                    DeferredPhysical::default(),
                ),
                tail: JCons {
                    head: StageRuntimeState::with_index(
                        |_: &u32, value: &i32| *value % 7,
                        crate::traits::RequiredRightKey(|_: &u32, value: &i32| *value % 7),
                        DirectProject(|_: &u32, left: &i32, _: &[(u32, i32)]| *left),
                        DeferredPhysical::default(),
                    ),
                    tail: JNil,
                },
            },
        };
        let initial = MapDiff::Initial {
            entries: vec![(1, 1), (2, 2), (4, 4), (8, 8)],
        };
        let mut sequential = RegionRouter::with_config(make_runtime(), 1, 1);
        let mut sharded = RegionRouter::with_config(make_runtime(), 3, 1);
        assert_eq!(
            sharded.apply_left(&initial),
            sequential.apply_left(&initial)
        );
        let events = [
            MapDiff::Insert {
                key: 9_u8,
                value: (Some(1), 10),
            },
            MapDiff::Update {
                key: 9,
                old_value: (Some(1), 10),
                new_value: (None, 10),
            },
            MapDiff::Update {
                key: 9,
                old_value: (None, 10),
                new_value: (Some(2), 11),
            },
        ];
        for event in &events {
            assert_eq!(
                sharded.apply_right::<Here, _, _>(event, true),
                sequential.apply_right::<Here, _, _>(event, true),
            );
        }
    }

    #[test]
    fn region_router_initial_replacement_writes_shared_index_once_and_invalidates_old_bucket() {
        #[derive(Clone)]
        struct CountingIndex {
            inner: DeferredPhysical<RelationIndex<u32, i32, u32>>,
            writes: Arc<std::sync::atomic::AtomicUsize>,
        }
        impl RelationIndexStorage<u32, i32, u32> for CountingIndex {
            type Read<'a> = parking_lot::RwLockReadGuard<'a, RelationIndex<u32, i32, u32>>;
            fn acquire_read(&self) -> Self::Read<'_> {
                self.inner.acquire_read()
            }
            fn write<T>(
                &mut self,
                write: impl FnOnce(&mut RelationIndex<u32, i32, u32>) -> T,
            ) -> T {
                self.writes
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                self.inner.write(write)
            }
        }
        let writes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let index = CountingIndex {
            inner: DeferredPhysical::default(),
            writes: Arc::clone(&writes),
        };
        let make_stage = |storage: CountingIndex| {
            StageRuntimeState::with_index(
                |_: &u32, value: &i32| value.unsigned_abs(),
                crate::traits::RequiredRightKey(|_: &u32, value: &i32| value.unsigned_abs()),
                DirectProject(|_: &u32, left: &i32, rights: &[(u32, i32)]| {
                    *left + rights.iter().map(|(_, value)| *value).sum::<i32>()
                }),
                storage,
            )
        };
        let runtime = JCons {
            head: make_stage(index.clone()),
            tail: JCons {
                head: make_stage(index.clone()),
                tail: JCons {
                    head: make_stage(index),
                    tail: JNil,
                },
            },
        };
        let mut router = RegionRouter::with_config(runtime, 3, 1);
        let _ = router.apply_left(&MapDiff::Initial {
            entries: vec![(1, 1), (2, 1), (3, 2)],
        });
        let _ = router.apply_right::<Here, _, _>(
            &MapDiff::Initial {
                entries: vec![(10, 1)],
            },
            true,
        );
        assert_eq!(writes.load(std::sync::atomic::Ordering::Relaxed), 1);
        let changes = router.apply_right::<Here, _, _>(
            &MapDiff::Initial {
                entries: vec![(20, 2)],
            },
            true,
        );
        assert_eq!(writes.load(std::sync::atomic::Ordering::Relaxed), 2);
        assert!(
            !changes.is_empty(),
            "old-only join bucket must be invalidated"
        );
    }

    #[test]
    #[allow(clippy::as_conversions)]
    fn materialized_region_preserves_repeated_and_interleaved_batch_members() {
        let left = crate::CellMap::<u32, i32>::new();
        let right = crate::CellMap::<u8, i32>::new();
        let stage = JoinStage::new(
            right,
            (|_: &u32, value: &i32| *value) as fn(&u32, &i32) -> i32,
            crate::traits::RequiredRightKey((|_: &u8, value: &i32| *value) as fn(&u8, &i32) -> i32),
            DirectProject(
                (|_: &u32, left: &i32, _: &[(u8, i32)]| *left)
                    as fn(&u32, &i32, &[(u8, i32)]) -> i32,
            ),
        );
        let output = JoinRegion::new(left.clone(), JNil.push(stage)).materialize();
        let emitted = output.diffs().materialize();

        left.apply_batch(
            (0..7_000)
                .map(|key| MapDiff::Insert {
                    key,
                    value: i32::try_from(key).unwrap_or(i32::MAX),
                })
                .collect(),
        );
        left.apply_batch(vec![
            MapDiff::Insert {
                key: 10_000,
                value: 1,
            },
            MapDiff::Insert {
                key: 20_000,
                value: 7,
            },
            MapDiff::Update {
                key: 10_000,
                old_value: 1,
                new_value: 2,
            },
            MapDiff::Remove {
                key: 10_000,
                old_value: 2,
            },
        ]);

        assert_eq!(
            emitted.get(),
            MapDiff::Batch {
                changes: vec![
                    MapDiff::Insert {
                        key: 10_000,
                        value: 1
                    },
                    MapDiff::Insert {
                        key: 20_000,
                        value: 7
                    },
                    MapDiff::Update {
                        key: 10_000,
                        old_value: 1,
                        new_value: 2,
                    },
                    MapDiff::Remove {
                        key: 10_000,
                        old_value: 2,
                    },
                ],
            }
        );
        assert_eq!(output.get_value(&10_000), None);
        assert_eq!(output.get_value(&20_000), Some(7));
    }

    fn panic_text(payload: &(dyn std::any::Any + Send)) -> Option<&str> {
        payload
            .downcast_ref::<&'static str>()
            .copied()
            .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
    }

    #[test]
    #[allow(clippy::expect_used, clippy::panic)]
    fn sequential_apply_panic_quarantines_left_and_right_callbacks() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let projections = Arc::new(AtomicUsize::new(0));
        let right_keys = Arc::new(AtomicUsize::new(0));
        let projection_count = Arc::clone(&projections);
        let right_key_count = Arc::clone(&right_keys);
        let runtime = JCons {
            head: StageRuntimeState::new(
                |_: &u32, value: &i32| *value,
                crate::traits::RequiredRightKey(move |_: &u8, value: &i32| {
                    right_key_count.fetch_add(1, Ordering::SeqCst);
                    *value
                }),
                DirectProject(move |_: &u32, left: &i32, _: &[(u8, i32)]| {
                    projection_count.fetch_add(1, Ordering::SeqCst);
                    if *left == 13 {
                        std::panic::panic_any("original sequential projection panic");
                    }
                    *left
                }),
            ),
            tail: JNil,
        };
        let state = Arc::new(Mutex::new(RegionRouter::with_config(runtime, 1, 100)));
        let original = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let changes = region_transaction(&state, |router| {
                router.apply_left(&MapDiff::Insert { key: 1, value: 13 })
            });
            // Publication is intentionally outside the transaction helper.
            let _published = changes.len();
        }))
        .expect_err("projection must panic");
        assert_eq!(
            panic_text(original.as_ref()),
            Some("original sequential projection panic")
        );
        assert!(state.lock().expect("unpoisoned mutex").poisoned);

        let projection_before = projections.load(Ordering::SeqCst);
        let right_before = right_keys.load(Ordering::SeqCst);
        for attempt in [0_u8, 1] {
            let poison = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                if attempt == 0 {
                    let _ = region_transaction(&state, |router| {
                        router.apply_left(&MapDiff::Insert { key: 2, value: 2 })
                    });
                } else {
                    let _ = region_transaction(&state, |router| {
                        router.apply_right::<Here, _, _>(
                            &MapDiff::Insert {
                                key: 1_u8,
                                value: 1_i32,
                            },
                            true,
                        )
                    });
                }
            }))
            .expect_err("poisoned region must fail fast");
            assert_eq!(panic_text(poison.as_ref()), Some(QUERY_POISONED_MESSAGE));
        }
        assert_eq!(projections.load(Ordering::SeqCst), projection_before);
        assert_eq!(right_keys.load(Ordering::SeqCst), right_before);
        assert!(!state.lock().expect("unpoisoned mutex").parallel_active);
    }

    #[test]
    #[allow(clippy::expect_used, clippy::panic)]
    fn right_maintainer_write_then_projection_panic_quarantines_region() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

        #[derive(Clone)]
        struct CountingIndex {
            inner: DeferredPhysical<RelationIndex<u8, i32, i32>>,
            writes: Arc<AtomicUsize>,
        }
        impl RelationIndexStorage<u8, i32, i32> for CountingIndex {
            type Read<'a> = parking_lot::RwLockReadGuard<'a, RelationIndex<u8, i32, i32>>;

            fn acquire_read(&self) -> Self::Read<'_> {
                self.inner.acquire_read()
            }

            fn write<T>(&mut self, write: impl FnOnce(&mut RelationIndex<u8, i32, i32>) -> T) -> T {
                self.writes.fetch_add(1, Ordering::SeqCst);
                self.inner.write(write)
            }
        }

        let writes = Arc::new(AtomicUsize::new(0));
        let panic_projection = Arc::new(AtomicBool::new(false));
        let panic_flag = Arc::clone(&panic_projection);
        let runtime = JCons {
            head: StageRuntimeState::with_index(
                |_: &u32, value: &i32| *value,
                crate::traits::RequiredRightKey(|_: &u8, value: &i32| *value),
                DirectProject(move |_: &u32, left: &i32, _: &[(u8, i32)]| {
                    if panic_flag.load(Ordering::SeqCst) {
                        std::panic::panic_any("right projection panic after write");
                    }
                    *left
                }),
                CountingIndex {
                    inner: DeferredPhysical::default(),
                    writes: Arc::clone(&writes),
                },
            ),
            tail: JNil,
        };
        let state = Arc::new(Mutex::new(RegionRouter::with_config(runtime, 1, 100)));
        let _ = region_transaction(&state, |router| {
            router.apply_left(&MapDiff::Insert { key: 1, value: 4 })
        });
        panic_projection.store(true, Ordering::SeqCst);
        let payload = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = region_transaction(&state, |router| {
                router.apply_right::<Here, _, _>(
                    &MapDiff::Insert {
                        key: 8_u8,
                        value: 4_i32,
                    },
                    true,
                )
            });
        }))
        .expect_err("right-root projection must panic");
        assert_eq!(
            panic_text(payload.as_ref()),
            Some("right projection panic after write")
        );
        assert_eq!(writes.load(Ordering::SeqCst), 1);
        assert!(state.lock().expect("unpoisoned mutex").poisoned);

        let poison = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = region_transaction(&state, |router| {
                router.apply_right::<Here, _, _>(
                    &MapDiff::Insert {
                        key: 9_u8,
                        value: 4_i32,
                    },
                    true,
                )
            });
        }))
        .expect_err("later right event must fail fast");
        assert_eq!(panic_text(poison.as_ref()), Some(QUERY_POISONED_MESSAGE));
        assert_eq!(writes.load(Ordering::SeqCst), 1);
    }

    #[test]
    #[cfg(all(feature = "scheduler", not(target_arch = "wasm32")))]
    #[allow(
        clippy::expect_used,
        clippy::items_after_statements,
        clippy::panic,
        clippy::significant_drop_tightening
    )]
    fn parallel_shard_panic_joins_siblings_and_publishes_nothing() {
        use std::{
            collections::HashSet,
            sync::{
                Barrier,
                atomic::{AtomicUsize, Ordering},
            },
        };

        if crate::executor::configured_worker_threads() <= 1 {
            return;
        }
        let shard = |key: &u32| {
            let mut hasher = FxHasher::default();
            key.hash(&mut hasher);
            usize::try_from(hasher.finish() % 4).unwrap_or(0)
        };
        let panic_key = (0..7_000)
            .find(|key| shard(key) == 0)
            .expect("shard zero key");
        let sibling_key = (0..7_000)
            .find(|key| shard(key) == 1)
            .expect("shard one key");
        let barrier = Arc::new(Barrier::new(2));
        let active = Arc::new(AtomicUsize::new(0));
        let sibling_commits = Arc::new(AtomicUsize::new(0));
        let workers = Arc::new(Mutex::new(HashSet::new()));
        let barrier_in = Arc::clone(&barrier);
        let active_in = Arc::clone(&active);
        let sibling_in = Arc::clone(&sibling_commits);
        let workers_in = Arc::clone(&workers);

        struct Active(Arc<AtomicUsize>);
        impl Drop for Active {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::SeqCst);
            }
        }

        let runtime = JCons {
            head: StageRuntimeState::new(
                |_: &u32, value: &i32| *value,
                crate::traits::RequiredRightKey(|_: &u8, value: &i32| *value),
                DirectProject(|_: &u32, left: &i32, _: &[(u8, i32)]| *left),
            ),
            tail: JCons {
                head: StageRuntimeState::new(
                    |_: &u32, value: &i32| *value,
                    crate::traits::RequiredRightKey(|_: &u16, value: &i32| *value),
                    DirectProject(move |key: &u32, left: &i32, _: &[(u16, i32)]| {
                        if *key == panic_key || *key == sibling_key {
                            active_in.fetch_add(1, Ordering::SeqCst);
                            let _active = Active(Arc::clone(&active_in));
                            if let Some(name) = std::thread::current().name() {
                                workers_in
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                                    .insert(name.to_owned());
                            }
                            barrier_in.wait();
                            if *key == panic_key {
                                std::panic::panic_any("parallel shard projection panic");
                            }
                            sibling_in.fetch_add(1, Ordering::SeqCst);
                        }
                        *left
                    }),
                ),
                tail: JNil,
            },
        };
        let state = Arc::new(Mutex::new(RegionRouter::with_config(runtime, 4, 1)));
        let published = AtomicUsize::new(0);
        let payload = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let changes =
                region_transaction(&state, |router| router.apply_left(&insert_batch(7_000)));
            published.fetch_add(changes.len(), Ordering::SeqCst);
        }))
        .expect_err("one parallel shard must panic");
        assert_eq!(
            panic_text(payload.as_ref()),
            Some("parallel shard projection panic")
        );
        assert_eq!(published.load(Ordering::SeqCst), 0);
        assert_eq!(active.load(Ordering::SeqCst), 0, "all Rayon work joined");
        assert!(sibling_commits.load(Ordering::SeqCst) > 0);
        assert!(
            workers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len()
                > 1,
            "barrier participants must run on sibling workers"
        );
        let router = state.lock().expect("unpoisoned mutex");
        assert!(router.poisoned);
        assert!(!router.parallel_active);
        assert!(router.last_left_workers.is_empty());
    }

    #[test]
    #[cfg(all(feature = "scheduler", not(target_arch = "wasm32")))]
    #[allow(
        clippy::as_conversions,
        clippy::expect_used,
        clippy::items_after_statements,
        clippy::panic,
        clippy::too_many_lines
    )]
    fn public_parallel_region_panic_publishes_no_materialized_rows() {
        use std::{
            collections::HashSet,
            sync::{
                Barrier,
                atomic::{AtomicUsize, Ordering},
            },
        };

        if crate::executor::configured_worker_threads() <= 1 {
            return;
        }
        struct Active(Arc<AtomicUsize>);
        impl Drop for Active {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::SeqCst);
            }
        }

        let shard_count = configured_shards();
        let shard = |key: &u32| {
            let mut hasher = FxHasher::default();
            key.hash(&mut hasher);
            let count = u64::try_from(shard_count).unwrap_or(1);
            usize::try_from(hasher.finish() % count).unwrap_or(0)
        };
        let panic_key = (0..10_000)
            .find(|key| shard(key) == 0)
            .expect("first shard key");
        let sibling_key = (0..10_000)
            .find(|key| shard(key) == 1)
            .expect("sibling shard key");
        let barrier = Arc::new(Barrier::new(2));
        let active = Arc::new(AtomicUsize::new(0));
        let sibling_commits = Arc::new(AtomicUsize::new(0));
        let workers = Arc::new(Mutex::new(HashSet::new()));
        let barrier_in = Arc::clone(&barrier);
        let active_in = Arc::clone(&active);
        let sibling_in = Arc::clone(&sibling_commits);
        let workers_in = Arc::clone(&workers);

        let left = crate::CellMap::<u32, i32>::new();
        let right = crate::CellMap::<u8, i32>::new();
        let identity_stage = || {
            JoinStage::new(
                right.clone(),
                (|_: &u32, value: &i32| *value) as fn(&u32, &i32) -> i32,
                crate::traits::RequiredRightKey(
                    (|_: &u8, value: &i32| *value) as fn(&u8, &i32) -> i32,
                ),
                DirectProject(
                    (|_: &u32, left: &i32, _: &[(u8, i32)]| *left)
                        as fn(&u32, &i32, &[(u8, i32)]) -> i32,
                ),
            )
        };
        let final_stage = JoinStage::new(
            right.clone(),
            (|_: &u32, value: &i32| *value) as fn(&u32, &i32) -> i32,
            crate::traits::RequiredRightKey((|_: &u8, value: &i32| *value) as fn(&u8, &i32) -> i32),
            DirectProject(move |key: &u32, left: &i32, _: &[(u8, i32)]| {
                if *key == panic_key || *key == sibling_key {
                    active_in.fetch_add(1, Ordering::SeqCst);
                    let _active = Active(Arc::clone(&active_in));
                    if let Some(name) = std::thread::current().name() {
                        workers_in
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .insert(name.to_owned());
                    }
                    barrier_in.wait();
                    if *key == panic_key {
                        std::panic::panic_any("public parallel projection panic");
                    }
                    sibling_in.fetch_add(1, Ordering::SeqCst);
                }
                *left
            }),
        );
        let stages = JNil
            .push(identity_stage())
            .push(identity_stage())
            .push(identity_stage())
            .push(final_stage);
        let output = JoinRegion::new(left.clone(), stages).materialize();
        let payload = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            left.apply_batch(
                (0..10_000)
                    .map(|key| MapDiff::Insert {
                        key,
                        value: i32::try_from(key).unwrap_or(i32::MAX),
                    })
                    .collect(),
            );
        }))
        .expect_err("public parallel projection must panic");
        assert_eq!(
            panic_text(payload.as_ref()),
            Some("public parallel projection panic")
        );
        assert_eq!(
            output.len().materialize().get(),
            0,
            "failed batch published no rows"
        );
        assert_eq!(active.load(Ordering::SeqCst), 0, "all Rayon work joined");
        assert!(sibling_commits.load(Ordering::SeqCst) > 0);
        assert!(
            workers
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len()
                > 1
        );
    }

    #[test]
    #[allow(clippy::expect_used, clippy::panic)]
    fn public_reentrant_event_is_cleared_then_fresh_event_hits_region_poison() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let left = crate::CellMap::<u32, i32>::new();
        let right = crate::CellMap::<u8, i32>::new();
        let reentrant_left = left.clone();
        let projections = Arc::new(AtomicUsize::new(0));
        let right_keys = Arc::new(AtomicUsize::new(0));
        let projection_count = Arc::clone(&projections);
        let right_key_count = Arc::clone(&right_keys);
        let stage = JoinStage::new(
            right.clone(),
            |_: &u32, value: &i32| *value,
            crate::traits::RequiredRightKey(move |_: &u8, value: &i32| {
                right_key_count.fetch_add(1, Ordering::SeqCst);
                *value
            }),
            DirectProject(move |key: &u32, left_value: &i32, _: &[(u8, i32)]| {
                projection_count.fetch_add(1, Ordering::SeqCst);
                if *key == 1 {
                    reentrant_left.insert(2, 2);
                    std::panic::panic_any("public region projection panic");
                }
                *left_value
            }),
        );
        let output = JoinRegion::new(left.clone(), JNil.push(stage)).materialize();
        let original = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            left.insert(1, 1);
        }))
        .expect_err("public callback must propagate original panic");
        assert_eq!(
            panic_text(original.as_ref()),
            Some("public region projection panic")
        );
        assert_eq!(projections.load(Ordering::SeqCst), 1);
        assert_eq!(output.get_value(&1), None);
        assert_eq!(output.get_value(&2), None, "queued event was discarded");

        let other_root_poison = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            right.insert(7, 7);
        }))
        .expect_err("a different physical root in the query must fail fast");
        assert_eq!(
            panic_text(other_root_poison.as_ref()),
            Some(QUERY_POISONED_MESSAGE)
        );
        assert_eq!(right_keys.load(Ordering::SeqCst), 0);

        let poison = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            left.insert(3, 3);
        }))
        .expect_err("fresh event must reach fail-fast poison");
        assert_eq!(panic_text(poison.as_ref()), Some(QUERY_POISONED_MESSAGE));
        assert_eq!(projections.load(Ordering::SeqCst), 1);
        assert_eq!(output.get_value(&3), None);
    }

    #[test]
    #[allow(clippy::expect_used, clippy::panic)]
    fn promotion_replay_panic_quarantines_region() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let panic_on_replay = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&panic_on_replay);
        let runtime = JCons {
            head: StageRuntimeState::new(
                |_: &u32, value: &i32| *value,
                crate::traits::RequiredRightKey(|_: &u8, value: &i32| *value),
                DirectProject(move |key: &u32, left: &i32, _: &[(u8, i32)]| {
                    if *key == 7 && flag.load(Ordering::SeqCst) {
                        std::panic::panic_any("promotion replay panic");
                    }
                    *left
                }),
            ),
            tail: JNil,
        };
        let state = Arc::new(Mutex::new(RegionRouter::with_config(runtime, 3, 2)));
        let _ = region_transaction(&state, |router| {
            router.apply_left(&MapDiff::Insert { key: 7, value: 7 })
        });
        panic_on_replay.store(true, Ordering::SeqCst);
        let payload = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = region_transaction(&state, |router| {
                router.apply_left(&MapDiff::Batch {
                    changes: vec![
                        MapDiff::Insert { key: 8, value: 8 },
                        MapDiff::Insert { key: 9, value: 9 },
                    ],
                })
            });
        }))
        .expect_err("promotion replay must panic");
        assert_eq!(panic_text(payload.as_ref()), Some("promotion replay panic"));
        let router = state.lock().expect("unpoisoned mutex");
        assert!(router.poisoned);
        assert!(!router.parallel_active);
        drop(router);
    }

    #[test]
    #[allow(clippy::expect_used, clippy::panic)]
    fn later_stage_panic_quarantines_the_whole_typed_spine() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let first_stage = Arc::new(AtomicUsize::new(0));
        let first_count = Arc::clone(&first_stage);
        let runtime = JCons {
            head: StageRuntimeState::new(
                |_: &u32, value: &i32| *value,
                crate::traits::RequiredRightKey(|_: &u8, value: &i32| *value),
                DirectProject(move |_: &u32, left: &i32, _: &[(u8, i32)]| {
                    first_count.fetch_add(1, Ordering::SeqCst);
                    *left
                }),
            ),
            tail: JCons {
                head: StageRuntimeState::new(
                    |_: &u32, value: &i32| *value,
                    crate::traits::RequiredRightKey(|_: &u16, value: &i32| *value),
                    DirectProject(|_: &u32, _: &i32, _: &[(u16, i32)]| {
                        std::panic::panic_any("tail projection panic");
                    }),
                ),
                tail: JNil,
            },
        };
        let state = Arc::new(Mutex::new(RegionRouter::with_config(runtime, 1, 100)));
        let published = Arc::new(AtomicUsize::new(0));
        let published_after = Arc::clone(&published);
        let payload = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let changes = region_transaction(&state, |router| {
                router.apply_left(&MapDiff::Insert { key: 1, value: 1 })
            });
            published_after.fetch_add(changes.len(), Ordering::SeqCst);
        }))
        .expect_err("tail stage must panic");
        assert_eq!(panic_text(payload.as_ref()), Some("tail projection panic"));
        assert!(first_stage.load(Ordering::SeqCst) > 0);
        assert_eq!(published.load(Ordering::SeqCst), 0);
        assert!(state.lock().expect("unpoisoned mutex").poisoned);
    }

    #[allow(clippy::as_conversions)]
    fn identity_runtime() -> JCons<
        StageRuntimeState<
            u32,
            i32,
            u8,
            i32,
            i32,
            i32,
            fn(&u32, &i32) -> i32,
            crate::traits::RequiredRightKey<fn(&u8, &i32) -> i32>,
            DirectProject<fn(&u32, &i32, &[(u8, i32)]) -> i32>,
        >,
        JNil,
    > {
        JCons {
            head: StageRuntimeState::new(
                (|_: &u32, value: &i32| *value) as fn(&u32, &i32) -> i32,
                crate::traits::RequiredRightKey(
                    (|_: &u8, value: &i32| *value) as fn(&u8, &i32) -> i32,
                ),
                DirectProject(
                    (|_: &u32, left: &i32, _: &[(u8, i32)]| *left)
                        as fn(&u32, &i32, &[(u8, i32)]) -> i32,
                ),
            ),
            tail: JNil,
        }
    }

    fn insert_batch(count: u32) -> MapDiff<u32, i32> {
        MapDiff::Batch {
            changes: (0..count)
                .map(|key| MapDiff::Insert {
                    key,
                    value: i32::try_from(key).unwrap_or(i32::MAX),
                })
                .collect(),
        }
    }

    #[test]
    fn production_sequential_and_promoted_initial_replacement_match_exactly() {
        let mut sequential = RegionRouter::new(identity_runtime());
        sequential.shard_count = 1;
        let mut promoted = RegionRouter::new(identity_runtime());
        promoted.shard_count = 3;
        promoted.promotion_work = 1;

        let old = MapDiff::Initial {
            entries: vec![(40, 40), (10, 10), (30, 30), (20, 20)],
        };
        assert_eq!(sequential.apply_left(&old), promoted.apply_left(&old));
        let replacement = MapDiff::Initial {
            // 40 and 20 are old-only, 10 overlaps, 50 and 5 are new.
            entries: vec![(10, 100), (50, 50), (5, 5)],
        };
        assert_eq!(
            sequential.apply_left(&replacement),
            promoted.apply_left(&replacement)
        );
    }

    #[test]
    #[cfg(feature = "scheduler")]
    fn region_parallel_policy_has_measured_hysteresis() {
        let mut router = RegionRouter::with_config(identity_runtime(), 4, 8_192);
        let _ = router.apply_left(&insert_batch(9_000));
        assert!(
            router.parallel_active,
            "large typed work enters parallel mode"
        );
        let _ = router.apply_left(&MapDiff::Batch {
            changes: (0..5_000)
                .map(|key| MapDiff::Update {
                    key,
                    old_value: i32::try_from(key).unwrap_or(i32::MAX),
                    new_value: i32::try_from(key).unwrap_or(i32::MAX).saturating_add(1),
                })
                .collect(),
        });
        assert!(router.parallel_active, "work inside the band stays active");
        let _ = router.apply_left(&MapDiff::Batch {
            changes: (0_i32..9_000)
                .map(|step| MapDiff::Update {
                    key: 0,
                    old_value: step,
                    new_value: step.saturating_add(1),
                })
                .collect(),
        });
        assert!(!router.parallel_active, "skew disables active mode");
        let _ = router.apply_left(&MapDiff::Batch {
            changes: (0..9_000)
                .map(|key| MapDiff::Update {
                    key,
                    old_value: i32::try_from(key).unwrap_or(i32::MAX),
                    new_value: i32::try_from(key).unwrap_or(i32::MAX).saturating_add(2),
                })
                .collect(),
        });
        assert!(router.parallel_active, "balanced work re-enters");
        let _ = router.apply_left(&MapDiff::Batch {
            changes: (0..3_000)
                .map(|key| MapDiff::Update {
                    key,
                    old_value: i32::try_from(key).unwrap_or(i32::MAX).saturating_add(1),
                    new_value: i32::try_from(key).unwrap_or(i32::MAX).saturating_add(2),
                })
                .collect(),
        });
        assert!(
            !router.parallel_active,
            "work below exit leaves parallel mode"
        );
    }

    #[test]
    fn typed_stage_cost_promotes_deeper_plan_at_equal_row_count() {
        let stage = || identity_runtime().head;
        let three = JCons {
            head: stage(),
            tail: JCons {
                head: stage(),
                tail: JCons {
                    head: stage(),
                    tail: JNil,
                },
            },
        };
        let eight = JCons {
            head: stage(),
            tail: JCons {
                head: stage(),
                tail: JCons {
                    head: stage(),
                    tail: JCons {
                        head: stage(),
                        tail: JCons {
                            head: stage(),
                            tail: JCons {
                                head: stage(),
                                tail: JCons {
                                    head: stage(),
                                    tail: JCons {
                                        head: stage(),
                                        tail: JNil,
                                    },
                                },
                            },
                        },
                    },
                },
            },
        };
        let rows = insert_batch(2_000);
        let mut shallow = RegionRouter::with_config(three, 4, 8_192);
        let mut deep = RegionRouter::with_config(eight, 4, 8_192);
        let _ = shallow.apply_left(&rows);
        let _ = deep.apply_left(&rows);
        assert!(shallow.shards.is_none());
        assert!(deep.shards.is_some());
    }

    #[test]
    #[cfg(all(feature = "scheduler", not(target_arch = "wasm32")))]
    fn balanced_large_left_batch_uses_dedicated_workers_but_tiny_stays_caller_thread() {
        if crate::executor::configured_worker_threads() <= 1 {
            return;
        }
        let mut router = RegionRouter::with_config(identity_runtime(), 4, 1);
        let _ = router.apply_left(&insert_batch(9_000));
        let workers: rustc_hash::FxHashSet<_> = router
            .last_left_workers
            .iter()
            .filter(|name| name.starts_with("hyphae-worker-"))
            .collect();
        assert!(
            workers.len() > 1,
            "balanced work must span dedicated workers"
        );

        let _ = router.apply_left(&MapDiff::Update {
            key: 0,
            old_value: 0,
            new_value: 1,
        });
        assert!(
            router
                .last_left_workers
                .iter()
                .all(|name| !name.starts_with("hyphae-worker-")),
            "tiny work must stay on the caller"
        );
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct DifferentialRight<const N: usize> {
        join: Option<i64>,
        add: i64,
    }

    macro_rules! differential_required_stage {
        ($n:literal, $modulus:literal) => {
            StageRuntimeState::with_index(
                |_: &u32, value: &i64| value.rem_euclid($modulus),
                RequiredRightKey(|_: &u16, value: &DifferentialRight<$n>| {
                    value.join.expect("required differential key")
                }),
                DirectProject(
                    |_: &u32, left: &i64, rights: &[(u16, DifferentialRight<$n>)]| {
                        *left + rights.iter().map(|(_, right)| right.add).sum::<i64>()
                    },
                ),
                DeferredPhysical::default(),
            )
        };
    }

    macro_rules! differential_optional_stage {
        ($n:literal, $modulus:literal) => {
            StageRuntimeState::with_index(
                |_: &u32, value: &i64| value.rem_euclid($modulus),
                OptionalRightKey(|_: &u16, value: &DifferentialRight<$n>| value.join),
                DirectProject(
                    |_: &u32, left: &i64, rights: &[(u16, DifferentialRight<$n>)]| {
                        *left + rights.iter().map(|(_, right)| right.add).sum::<i64>()
                    },
                ),
                DeferredPhysical::default(),
            )
        };
    }

    macro_rules! differential_runtime3 {
        () => {
            JNil.push(differential_required_stage!(0, 5))
                .push(differential_optional_stage!(1, 7))
                .push(differential_required_stage!(2, 11))
        };
    }

    macro_rules! differential_runtime8 {
        () => {
            JNil.push(differential_required_stage!(0, 5))
                .push(differential_optional_stage!(1, 7))
                .push(differential_required_stage!(2, 11))
                .push(differential_required_stage!(3, 13))
                .push(differential_optional_stage!(4, 17))
                .push(differential_required_stage!(5, 19))
                .push(differential_required_stage!(6, 23))
                .push(differential_optional_stage!(7, 29))
        };
    }

    fn apply_trace<K: Ord + Clone, V: Clone>(
        state: &mut std::collections::BTreeMap<K, V>,
        changes: &[MapDiff<K, V>],
    ) {
        fn apply<K: Ord + Clone, V: Clone>(
            state: &mut std::collections::BTreeMap<K, V>,
            change: &MapDiff<K, V>,
        ) {
            match change {
                MapDiff::Initial { entries } => {
                    state.clear();
                    state.extend(entries.iter().cloned());
                }
                MapDiff::Insert { key, value } => {
                    state.insert(key.clone(), value.clone());
                }
                MapDiff::Update { key, new_value, .. } => {
                    state.insert(key.clone(), new_value.clone());
                }
                MapDiff::Remove { key, .. } => {
                    state.remove(key);
                }
                MapDiff::Batch { changes } => {
                    for change in changes {
                        apply(state, change);
                    }
                }
            }
        }
        for change in changes {
            apply(state, change);
        }
    }

    use std::collections::BTreeMap;

    fn apply_input<V: Clone>(state: &mut BTreeMap<u32, V>, change: &MapDiff<u32, V>) {
        apply_trace(state, std::slice::from_ref(change));
    }

    fn apply_right_input<const N: usize>(
        states: &mut [BTreeMap<u16, (Option<i64>, i64)>; 8],
        change: &MapDiff<u16, DifferentialRight<N>>,
    ) {
        fn normalized<const N: usize>(
            change: &MapDiff<u16, DifferentialRight<N>>,
        ) -> MapDiff<u16, (Option<i64>, i64)> {
            match change {
                MapDiff::Initial { entries } => MapDiff::Initial {
                    entries: entries
                        .iter()
                        .map(|(key, value)| (*key, (value.join, value.add)))
                        .collect(),
                },
                MapDiff::Insert { key, value } => MapDiff::Insert {
                    key: *key,
                    value: (value.join, value.add),
                },
                MapDiff::Update {
                    key,
                    old_value,
                    new_value,
                } => MapDiff::Update {
                    key: *key,
                    old_value: (old_value.join, old_value.add),
                    new_value: (new_value.join, new_value.add),
                },
                MapDiff::Remove { key, old_value } => MapDiff::Remove {
                    key: *key,
                    old_value: (old_value.join, old_value.add),
                },
                MapDiff::Batch { changes } => MapDiff::Batch {
                    changes: changes.iter().map(normalized).collect(),
                },
            }
        }
        if let Some(state) = states.get_mut(N) {
            apply_trace(state, &[normalized(change)]);
        }
    }

    fn eager_differential8(
        left: &BTreeMap<u32, i64>,
        rights: &[BTreeMap<u16, (Option<i64>, i64)>; 8],
    ) -> BTreeMap<u32, i64> {
        const MODULI: [i64; 8] = [5, 7, 11, 13, 17, 19, 23, 29];
        left.iter()
            .map(|(key, initial)| {
                let mut value = *initial;
                for (right, modulus) in rights.iter().zip(MODULI) {
                    let join = value.rem_euclid(modulus);
                    value = value.wrapping_add(
                        right
                            .values()
                            .filter(|(right_join, _)| *right_join == Some(join))
                            .map(|(_, add)| *add)
                            .sum::<i64>(),
                    );
                }
                (*key, value)
            })
            .collect()
    }

    #[test]
    #[cfg(not(target_arch = "wasm32"))]
    #[allow(
        clippy::arithmetic_side_effects,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::too_many_lines
    )]
    fn forced_static_n8_serial_parallel_differential_is_exact_and_uses_injected_workers() {
        use crate::map_query::compiler::{TestRegionConfig, TestRegionDispatch};
        use std::collections::BTreeMap;

        let pool = Arc::new(
            rayon::ThreadPoolBuilder::new()
                .num_threads(4)
                .thread_name(|index| format!("join-region-audit-{index}"))
                .build()
                .expect("named injected Rayon pool"),
        );
        for shard_count in [3, 8] {
            let mut oracle = RegionRouter::new(differential_runtime8!());
            let mut serial = RegionRouter::with_test_config(
                differential_runtime8!(),
                QueryPoison::default(),
                TestRegionConfig {
                    shards: shard_count,
                    promote_after: 1,
                    dispatch: TestRegionDispatch::ForceSerial,
                },
            );
            let mut parallel = RegionRouter::with_test_config(
                differential_runtime8!(),
                QueryPoison::default(),
                TestRegionConfig {
                    shards: shard_count,
                    promote_after: 1,
                    dispatch: TestRegionDispatch::InjectedRayon(Arc::clone(&pool)),
                },
            );
            let mut oracle_state = BTreeMap::new();
            let mut serial_state = BTreeMap::new();
            let mut parallel_state = BTreeMap::new();
            let mut eager_left = BTreeMap::new();
            let mut eager_rights: [BTreeMap<u16, (Option<i64>, i64)>; 8] =
                std::array::from_fn(|_| BTreeMap::new());

            macro_rules! left {
                ($change:expr, $label:literal) => {{
                    let change = $change;
                    apply_input(&mut eager_left, &change);
                    let expected = oracle.apply_left(&change);
                    let actual_serial = serial.apply_left(&change);
                    let actual_parallel = parallel.apply_left(&change);
                    assert_eq!(
                        actual_serial, expected,
                        "{}: shard {shard_count} serial trace",
                        $label
                    );
                    assert_eq!(
                        actual_parallel, expected,
                        "{}: shard {shard_count} Rayon trace",
                        $label
                    );
                    apply_trace(&mut oracle_state, &expected);
                    apply_trace(&mut serial_state, &actual_serial);
                    apply_trace(&mut parallel_state, &actual_parallel);
                    assert_eq!(serial_state, oracle_state, "{}: serial state", $label);
                    assert_eq!(parallel_state, oracle_state, "{}: Rayon state", $label);
                    assert_eq!(
                        oracle_state,
                        eager_differential8(&eager_left, &eager_rights),
                        "{}: independent eager state",
                        $label
                    );
                }};
            }
            macro_rules! right {
                ($location:ty, $rk:ty, $rv:ty, $change:expr, $label:literal) => {{
                    let change: MapDiff<$rk, $rv> = $change;
                    apply_right_input(&mut eager_rights, &change);
                    let expected = oracle.apply_right::<$location, $rk, $rv>(&change, true);
                    let actual_serial = serial.apply_right::<$location, $rk, $rv>(&change, true);
                    let actual_parallel =
                        parallel.apply_right::<$location, $rk, $rv>(&change, true);
                    assert_eq!(
                        actual_serial, expected,
                        "{}: shard {shard_count} serial trace",
                        $label
                    );
                    assert_eq!(
                        actual_parallel, expected,
                        "{}: shard {shard_count} Rayon trace",
                        $label
                    );
                    apply_trace(&mut oracle_state, &expected);
                    apply_trace(&mut serial_state, &actual_serial);
                    apply_trace(&mut parallel_state, &actual_parallel);
                    assert_eq!(serial_state, oracle_state, "{}: serial state", $label);
                    assert_eq!(parallel_state, oracle_state, "{}: Rayon state", $label);
                    assert_eq!(
                        oracle_state,
                        eager_differential8(&eager_left, &eager_rights),
                        "{}: independent eager state",
                        $label
                    );
                }};
            }

            // Right activation order precedes the left Initial. Every concrete
            // Here/There entry point has a distinct value type.
            right!(
                Here,
                u16,
                DifferentialRight<0>,
                MapDiff::Initial {
                    entries: vec![
                        (
                            1,
                            DifferentialRight {
                                join: Some(0),
                                add: 2
                            }
                        ),
                        (
                            2,
                            DifferentialRight {
                                join: Some(0),
                                add: 3
                            }
                        )
                    ]
                },
                "root0 Initial multiple matches"
            );
            right!(
                There<Here>,
                u16,
                DifferentialRight<1>,
                MapDiff::Initial {
                    entries: vec![
                        (
                            1,
                            DifferentialRight {
                                join: None,
                                add: 100
                            }
                        ),
                        (
                            2,
                            DifferentialRight {
                                join: Some(2),
                                add: 5
                            }
                        )
                    ]
                },
                "root1 optional Initial"
            );
            right!(
                There<There<Here>>,
                u16,
                DifferentialRight<2>,
                MapDiff::Initial {
                    entries: vec![(
                        1,
                        DifferentialRight {
                            join: Some(5),
                            add: 7
                        }
                    )]
                },
                "root2 Initial"
            );
            right!(
                There<There<There<Here>>>,
                u16,
                DifferentialRight<3>,
                MapDiff::Initial {
                    entries: vec![(
                        1,
                        DifferentialRight {
                            join: Some(12),
                            add: 11
                        }
                    )]
                },
                "root3 Initial"
            );
            right!(
                There<There<There<There<Here>>>>,
                u16,
                DifferentialRight<4>,
                MapDiff::Initial { entries: vec![] },
                "root4 empty Initial"
            );
            right!(
                There<There<There<There<There<Here>>>>>,
                u16,
                DifferentialRight<5>,
                MapDiff::Initial {
                    entries: vec![(
                        1,
                        DifferentialRight {
                            join: Some(4),
                            add: 13
                        }
                    )]
                },
                "root5 Initial"
            );
            right!(
                There<There<There<There<There<There<Here>>>>>>,
                u16,
                DifferentialRight<6>,
                MapDiff::Initial {
                    entries: vec![(
                        1,
                        DifferentialRight {
                            join: Some(17),
                            add: 17
                        }
                    )]
                },
                "root6 Initial"
            );
            right!(
                There<There<There<There<There<There<There<Here>>>>>>>,
                u16,
                DifferentialRight<7>,
                MapDiff::Initial {
                    entries: vec![(
                        1,
                        DifferentialRight {
                            join: None,
                            add: 19
                        }
                    )]
                },
                "root7 optional Initial"
            );

            let entries = (0..512_u32).map(|key| (key, i64::from(key % 31))).collect();
            left!(MapDiff::Initial { entries }, "balanced left Initial");
            let workers: rustc_hash::FxHashSet<_> = parallel.last_left_workers.iter().collect();
            assert!(
                workers.len() >= 2,
                "injected execution used only {workers:?}"
            );
            assert!(
                workers
                    .iter()
                    .all(|name| name.starts_with("join-region-audit-"))
            );

            left!(
                MapDiff::Batch {
                    changes: vec![
                        MapDiff::Update {
                            key: 3,
                            old_value: 3,
                            new_value: 40
                        },
                        MapDiff::Batch {
                            changes: vec![
                                MapDiff::Update {
                                    key: 3,
                                    old_value: 40,
                                    new_value: 41
                                },
                                MapDiff::Remove {
                                    key: 4,
                                    old_value: 4
                                },
                                MapDiff::Insert { key: 700, value: 9 },
                            ]
                        },
                        MapDiff::Initial {
                            entries: vec![(9, 9), (10, 10), (11, 11), (12, 12)]
                        },
                        MapDiff::Initial {
                            entries: vec![(12, 12), (9, 9), (11, 11), (10, 10)]
                        },
                        MapDiff::Update {
                            key: 9,
                            old_value: 9,
                            new_value: 29
                        },
                    ]
                },
                "nested Batch repeated key and Initial"
            );
            right!(
                There<Here>,
                u16,
                DifferentialRight<1>,
                MapDiff::Update {
                    key: 1,
                    old_value: DifferentialRight {
                        join: None,
                        add: 100
                    },
                    new_value: DifferentialRight {
                        join: Some(1),
                        add: 23
                    }
                },
                "optional None-Some rekey"
            );
            right!(
                There<Here>,
                u16,
                DifferentialRight<1>,
                MapDiff::Update {
                    key: 1,
                    old_value: DifferentialRight {
                        join: Some(1),
                        add: 23
                    },
                    new_value: DifferentialRight {
                        join: None,
                        add: 29
                    }
                },
                "optional Some-None"
            );
            right!(
                Here,
                u16,
                DifferentialRight<0>,
                MapDiff::Update {
                    key: 2,
                    old_value: DifferentialRight {
                        join: Some(0),
                        add: 3
                    },
                    new_value: DifferentialRight {
                        join: Some(4),
                        add: 31
                    }
                },
                "required right rekey"
            );
            right!(
                There<There<Here>>,
                u16,
                DifferentialRight<2>,
                MapDiff::Batch {
                    changes: vec![
                        MapDiff::Insert {
                            key: 2,
                            value: DifferentialRight {
                                join: Some(7),
                                add: 37
                            }
                        },
                        MapDiff::Update {
                            key: 2,
                            old_value: DifferentialRight {
                                join: Some(7),
                                add: 37
                            },
                            new_value: DifferentialRight {
                                join: Some(8),
                                add: 41
                            }
                        },
                        MapDiff::Remove {
                            key: 2,
                            old_value: DifferentialRight {
                                join: Some(8),
                                add: 41
                            }
                        },
                    ]
                },
                "right repeated key Batch"
            );
            right!(
                There<There<There<Here>>>,
                u16,
                DifferentialRight<3>,
                MapDiff::Initial {
                    entries: vec![(
                        8,
                        DifferentialRight {
                            join: Some(0),
                            add: 43
                        }
                    )]
                },
                "right Initial replacement"
            );
            right!(
                There<There<There<There<Here>>>>,
                u16,
                DifferentialRight<4>,
                MapDiff::Initial {
                    entries: vec![(
                        1,
                        DifferentialRight {
                            join: Some(0),
                            add: 47
                        }
                    )]
                },
                "optional root Initial replacement"
            );
            right!(
                There<There<There<There<There<Here>>>>>,
                u16,
                DifferentialRight<5>,
                MapDiff::Initial { entries: vec![] },
                "required root empty Initial"
            );
            right!(
                There<There<There<There<There<There<Here>>>>>>,
                u16,
                DifferentialRight<6>,
                MapDiff::Initial {
                    entries: vec![(
                        2,
                        DifferentialRight {
                            join: Some(0),
                            add: 53
                        }
                    )]
                },
                "root6 Initial replacement"
            );
            right!(
                There<There<There<There<There<There<There<Here>>>>>>>,
                u16,
                DifferentialRight<7>,
                MapDiff::Initial {
                    entries: vec![(
                        2,
                        DifferentialRight {
                            join: Some(0),
                            add: 59
                        }
                    )]
                },
                "root7 Initial replacement"
            );
            left!(
                MapDiff::Remove {
                    key: 9,
                    old_value: 29
                },
                "final atomic removal"
            );
            let mut generated_random = 0xd1b5_4a32_d192_ed03_u64;
            for generated_step in 0..8_u32 {
                generated_random ^= generated_random << 13;
                generated_random ^= generated_random >> 7;
                generated_random ^= generated_random << 17;
                let repeated_key = 1_000_u32.saturating_add(generated_step.saturating_mul(3));
                let other_key = repeated_key.saturating_add(1);
                let initial_value = i64::try_from(generated_random % 101).unwrap_or_default();
                let other_value = i64::try_from((generated_random >> 8) % 101).unwrap_or_default();
                let middle_value = initial_value
                    .saturating_add(i64::from(generated_step))
                    .saturating_add(1);
                let final_value = middle_value
                    .saturating_add(
                        i64::try_from((generated_random >> 16) % 17).unwrap_or_default(),
                    )
                    .saturating_add(1);
                let generated = MapDiff::Batch {
                    changes: vec![
                        MapDiff::Initial {
                            entries: vec![(other_key, other_value), (repeated_key, initial_value)],
                        },
                        MapDiff::Batch {
                            changes: vec![
                                MapDiff::Update {
                                    key: repeated_key,
                                    old_value: initial_value,
                                    new_value: middle_value,
                                },
                                MapDiff::Batch {
                                    changes: vec![MapDiff::Update {
                                        key: repeated_key,
                                        old_value: middle_value,
                                        new_value: final_value,
                                    }],
                                },
                            ],
                        },
                    ],
                };
                left!(generated, "seeded nested Initial and repeated-key Batch");
            }
            left!(
                MapDiff::Initial { entries: vec![] },
                "final empty left Initial"
            );
            assert!(oracle_state.is_empty());
        }
    }

    #[test]
    #[cfg(not(target_arch = "wasm32"))]
    #[allow(clippy::arithmetic_side_effects, clippy::unwrap_used)]
    #[allow(clippy::indexing_slicing, clippy::too_many_lines)]
    fn forced_static_n3_has_three_concrete_entry_points() {
        use crate::map_query::compiler::{TestRegionConfig, TestRegionDispatch};
        let pool = Arc::new(
            rayon::ThreadPoolBuilder::new()
                .num_threads(4)
                .build()
                .unwrap(),
        );
        let config = |dispatch| TestRegionConfig {
            shards: 3,
            promote_after: 1,
            dispatch,
        };
        let mut oracle = RegionRouter::new(differential_runtime3!());
        let mut serial = RegionRouter::with_test_config(
            differential_runtime3!(),
            QueryPoison::default(),
            config(TestRegionDispatch::ForceSerial),
        );
        let mut parallel = RegionRouter::with_test_config(
            differential_runtime3!(),
            QueryPoison::default(),
            config(TestRegionDispatch::InjectedRayon(pool)),
        );
        let a = MapDiff::Initial {
            entries: vec![(
                0,
                DifferentialRight {
                    join: Some(0),
                    add: 1,
                },
            )],
        };
        let expected = oracle.apply_right::<Here, u16, DifferentialRight<0>>(&a, true);
        assert_eq!(
            serial.apply_right::<Here, u16, DifferentialRight<0>>(&a, true),
            expected
        );
        assert_eq!(
            parallel.apply_right::<Here, u16, DifferentialRight<0>>(&a, true),
            expected
        );
        let b = MapDiff::Initial {
            entries: vec![(0, DifferentialRight { join: None, add: 2 })],
        };
        let expected = oracle.apply_right::<There<Here>, u16, DifferentialRight<1>>(&b, true);
        assert_eq!(
            serial.apply_right::<There<Here>, u16, DifferentialRight<1>>(&b, true),
            expected
        );
        assert_eq!(
            parallel.apply_right::<There<Here>, u16, DifferentialRight<1>>(&b, true),
            expected
        );
        let c = MapDiff::Initial {
            entries: vec![(
                0,
                DifferentialRight {
                    join: Some(1),
                    add: 3,
                },
            )],
        };
        let expected =
            oracle.apply_right::<There<There<Here>>, u16, DifferentialRight<2>>(&c, true);
        assert_eq!(
            serial.apply_right::<There<There<Here>>, u16, DifferentialRight<2>>(&c, true),
            expected
        );
        assert_eq!(
            parallel.apply_right::<There<There<Here>>, u16, DifferentialRight<2>>(&c, true),
            expected
        );
        let left = MapDiff::Initial {
            entries: (0..256).map(|key| (key, i64::from(key))).collect(),
        };
        let expected = oracle.apply_left(&left);
        assert_eq!(serial.apply_left(&left), expected);
        assert_eq!(parallel.apply_left(&left), expected);

        // Fixed seed table: every seed drives a deterministic sequence whose
        // old_values are interpreted against the immediately preceding leaf.
        let mut values: Vec<i64> = (0..256).map(i64::from).collect();
        for seed in [0x243f_6a88_u64, 0x85a3_08d3, 0x1319_8a2e] {
            let mut random = seed;
            for step in 0..96_u32 {
                random ^= random << 13;
                random ^= random >> 7;
                random ^= random << 17;
                let key = u32::try_from(random % 256).unwrap_or_default();
                let index = usize::try_from(key).unwrap_or_default();
                let old_value = values[index];
                let new_value = i64::from(step) + i64::try_from(random % 97).unwrap_or_default();
                values[index] = new_value;
                let change = MapDiff::Update {
                    key,
                    old_value,
                    new_value,
                };
                let expected = oracle.apply_left(&change);
                assert_eq!(
                    serial.apply_left(&change),
                    expected,
                    "seed={seed:#x} step={step} serial"
                );
                assert_eq!(
                    parallel.apply_left(&change),
                    expected,
                    "seed={seed:#x} step={step} Rayon"
                );
            }
        }
    }

    #[test]
    #[cfg(not(target_arch = "wasm32"))]
    #[allow(clippy::expect_used)]
    fn shared_relationship_maintainer_writes_exactly_once_per_physical_leaf() {
        use crate::{
            map_query::compiler::{TestRegionConfig, TestRegionDispatch},
            traits::DepNode as _,
        };
        use std::sync::atomic::{AtomicUsize, Ordering};

        let left = crate::CellMap::<u32, u32>::new();
        let right = SharedRight::new();
        let baseline_left = left.inner.diffs_cell.subscriber_count();
        let baseline_right = right.inner.diffs_cell.subscriber_count();
        let probe = Arc::new(AtomicUsize::new(0));
        let pool = Arc::new(
            rayon::ThreadPoolBuilder::new()
                .num_threads(4)
                .thread_name(|index| format!("join-region-maintainer-{index}"))
                .build()
                .expect("maintainer test pool"),
        );
        let stages = JNil
            .push(shared_stage(right.clone()))
            .push(shared_stage(right.clone()))
            .push(shared_stage(right.clone()));
        let mut cx = CompileContext::default();
        cx.set_test_region_config(TestRegionConfig {
            shards: 8,
            promote_after: 1,
            dispatch: TestRegionDispatch::InjectedRayon(pool),
        });
        cx.set_maintained_write_probe(Arc::clone(&probe));
        let mut guards = JoinRegion::new(left.clone(), stages).build_into(&mut cx, |_| {});
        guards.extend(cx.activate());
        // A physical root's subscription delivers one Initial callback. Its
        // entry count is irrelevant: the maintained index is replaced by one
        // write for that callback, matching the physical callback contract.
        assert_eq!(probe.load(Ordering::Relaxed), 1);
        assert_eq!(
            right.inner.diffs_cell.subscriber_count(),
            baseline_right + 1
        );
        assert_eq!(left.inner.diffs_cell.subscriber_count(), baseline_left + 1);

        right.insert(1, (7, 10));
        assert_eq!(probe.load(Ordering::Relaxed), 2);
        right.apply_batch(vec![
            MapDiff::Update {
                key: 1,
                old_value: (7, 10),
                new_value: (8, 11),
            },
            MapDiff::Insert {
                key: 2,
                value: (7, 12),
            },
            MapDiff::Remove {
                key: 2,
                old_value: (7, 12),
            },
        ]);
        assert_eq!(probe.load(Ordering::Relaxed), 5);
        right.apply_diff_owned(MapDiff::Initial {
            entries: vec![(3, (9, 13)), (4, (9, 14))],
        });
        assert_eq!(
            probe.load(Ordering::Relaxed),
            6,
            "Initial is one physical leaf"
        );

        drop(guards);
        assert_eq!(right.inner.diffs_cell.subscriber_count(), baseline_right);
        assert_eq!(left.inner.diffs_cell.subscriber_count(), baseline_left);
        let settled = probe.load(Ordering::Relaxed);
        left.insert(99, 7);
        right.insert(99, (7, 99));
        assert_eq!(
            probe.load(Ordering::Relaxed),
            settled,
            "teardown leaves no work"
        );
    }
}
