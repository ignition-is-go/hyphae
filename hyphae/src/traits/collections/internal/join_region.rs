#![allow(dead_code)]

//! Static type substrate for an arbitrary-length left-join region.
//!
//! A join region is described and installed without tuples (and therefore
//! without tuple-arity limits), while every stage, right-root entry, and
//! projection remains statically dispatched.
//!
//! Do not add direct deep `JCons` test instantiations here. Public composition
//! is automatically segmented into bounded typed regions, and its behavioral
//! coverage lives in the application-shaped integration tests. Instantiating
//! the legacy unbounded substrate in the monolithic lib-test crate can consume
//! tens of gigabytes during LLVM code generation.

use std::{
    any::TypeId,
    collections::hash_map::Entry,
    hash::{Hash, Hasher},
    marker::PhantomData,
    sync::Arc,
};

use rustc_hash::{FxHashMap, FxHasher};

use crate::{
    cell_map::MapDiff,
    map_query::{
        BuildQueryRuntime, MapQuery, compile_runtime_into,
        compiler::{CompileContext, DeferredPhysical},
        properties::{ByMapKey, Partition, PlanProperties, ZeroOrOne},
    },
    subscription::SubscriptionGuard,
    traits::{
        CellValue, ForeignKeyRelation, IdFor, OptionalRightKey, RequiredRightKey, RightJoinKey,
    },
};

use super::{
    join_lifecycle::{
        FailStopTransaction, InstallRegionRights, RegionHost, RootRegistrationOrder,
        RuntimeStorage, TransactionPolicy, install_region_runtime,
    },
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
    fn install<Right>(
        right: Right,
        cx: &mut CompileContext,
        binding: Option<Self::Binding>,
        sink: crate::map_query::BoxedMapDiffSink<RK, RV>,
    ) -> Vec<SubscriptionGuard>
    where
        Right: MapQuery<Key = RK, Value = RV>;
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

    fn install<Right>(
        right: Right,
        cx: &mut CompileContext,
        _: Option<Self::Binding>,
        sink: crate::map_query::BoxedMapDiffSink<RK, RV>,
    ) -> Vec<SubscriptionGuard>
    where
        Right: MapQuery<Key = RK, Value = RV>,
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

    fn install<Right>(
        right: Right,
        cx: &mut CompileContext,
        binding: Option<Self::Binding>,
        sink: crate::map_query::BoxedMapDiffSink<RK, RV>,
    ) -> Vec<SubscriptionGuard>
    where
        Right: MapQuery<Key = RK, Value = RV>,
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
    storage: RuntimeStorage<Runtime, Vec<Runtime>>,
    key_sequence: FxHashMap<K, u64>,
    next_sequence: u64,
    shard_count: usize,
    promotion_work: usize,
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
            storage: RuntimeStorage::Serial(runtime),
            key_sequence: FxHashMap::default(),
            next_sequence: 0,
            shard_count: configured_shards(),
            promotion_work: configured_promotion_work(),
            _input: PhantomData,
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
        let count = self.shard_count.max(1);
        let key_sequence = &self.key_sequence;
        self.storage.promote_with(|sequential| {
            let mut shards: Vec<_> = (0..count).map(|_| sequential.empty_shard()).collect();
            // Replay in stable source order. Outputs are intentionally discarded.
            let mut keys: Vec<_> = key_sequence.iter().collect();
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
            shards
        });
    }

    fn order_changes<Output: CellValue>(
        order: &FxHashMap<K, u64>,
        changes: &mut [MapDiff<K, Output>],
    ) {
        changes.sort_by_key(|change| {
            change
                .atomic_key()
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
                let key = other.atomic_key().expect("non-container diff has a key");
                routed[Self::shard_for(key, shard_count)].push((ordinal, other.clone()));
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
        let changes = self.storage.serial_mut().apply_left_diff(diff);
        let mut flat = Vec::new();
        for change in changes {
            change.flatten_into(&mut flat);
        }
        Self::order_changes(&order, &mut flat);
        output.extend(flat);
        self.remember(diff);
    }

    fn apply_serial_left(
        &mut self,
        diff: &MapDiff<K, Input>,
        eventwise: bool,
    ) -> Vec<MapDiff<K, Runtime::Output>> {
        #[cfg(feature = "region-calibration")]
        crate::region_calibration::left_serial_dispatch();
        if eventwise {
            let mut changes = Vec::new();
            self.apply_left_eventwise(diff, &mut changes);
            return vec![MapDiff::Batch { changes }];
        }
        if matches!(diff, MapDiff::Initial { .. }) {
            let order = self.merge_order(diff);
            let mut output = self.storage.serial_mut().apply_left_diff(diff);
            Self::order_changes(&order, &mut output);
            self.remember(diff);
            return output;
        }
        self.remember(diff);
        self.storage.serial_mut().apply_left_diff(diff)
    }

    fn apply_left(&mut self, diff: &MapDiff<K, Input>) -> Vec<MapDiff<K, Runtime::Output>> {
        let batch_is_unique = match diff {
            MapDiff::Batch { changes } => Some(batch_has_unique_atomic_keys(changes)),
            _ => None,
        };
        let non_unique_batch = matches!(batch_is_unique, Some(false));
        let is_serial = self.storage.is_serial();
        if is_serial && self.shard_count <= 1 {
            return self.apply_serial_left(diff, non_unique_batch);
        }
        let estimated_work = diff.work_items().saturating_mul(Runtime::COST_UNITS);
        let promotion_warranted = diff.work_items() >= self.promotion_work
            || estimated_work >= PARALLEL_REGION_WORK_ENTER;
        if is_serial && ((self.shard_count <= 1) || !promotion_warranted) {
            return self.apply_serial_left(diff, non_unique_batch);
        }
        if is_serial {
            self.promote();
        }

        let order = self.merge_order(diff);
        let event_orders = non_unique_batch.then(|| self.event_orders(diff));
        let preserve_batch = batch_is_unique.is_some();
        let unique_batch = batch_is_unique.unwrap_or(false);

        let (shards, parallel_active) = self.storage.sharded_mut();
        let mut routed = vec![Vec::new(); shards.len()];
        let mut next_ordinal = 0;
        Self::route_diff(diff, shards.len(), &mut next_ordinal, &mut routed);

        let hysteresis_wants_parallel = if *parallel_active {
            estimated_work >= PARALLEL_REGION_WORK_EXIT
        } else {
            estimated_work >= PARALLEL_REGION_WORK_ENTER
        };
        let shard_work: Vec<_> = routed
            .iter()
            .map(|changes| {
                changes.iter().fold(0_usize, |work, (_, change)| {
                    work.saturating_add(change.work_items())
                })
            })
            .collect();
        let active_shards = shard_work.iter().filter(|work| **work != 0).count();
        let max_shard_work = shard_work.iter().copied().max().unwrap_or(0);
        let balanced = active_shards > 1
            && max_shard_work.saturating_mul(4) <= diff.work_items().saturating_mul(3);
        #[cfg(not(all(feature = "scheduler", not(target_arch = "wasm32"))))]
        let _ = (hysteresis_wants_parallel, balanced);
        #[cfg(all(feature = "scheduler", not(target_arch = "wasm32")))]
        let resources_available =
            hysteresis_wants_parallel && balanced && crate::executor::worker_pool().is_some();
        #[cfg(not(all(feature = "scheduler", not(target_arch = "wasm32"))))]
        let resources_available = false;
        #[cfg(feature = "region-calibration")]
        let was_parallel = *parallel_active;
        *parallel_active = resources_available;
        #[cfg(feature = "region-calibration")]
        match (was_parallel, *parallel_active) {
            (false, true) => crate::region_calibration::inactive_to_parallel(),
            (true, false) => crate::region_calibration::parallel_to_inactive(),
            _ => {}
        }
        let run_parallel = *parallel_active;

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
                for change in shard.apply_left_diff(&batch) {
                    change.flatten_into(&mut flat);
                }
                for (local, change) in flat.into_iter().enumerate() {
                    let ordinal = change
                        .atomic_key()
                        .and_then(|key| order.get(key))
                        .copied()
                        .unwrap_or(u64::MAX);
                    tagged.push((ordinal, local, shard_id, change));
                }
            } else {
                for (ordinal, change) in changes {
                    let mut flat = Vec::new();
                    for change in shard.apply_left_diff(&change) {
                        change.flatten_into(&mut flat);
                    }
                    for (local, output) in flat.into_iter().enumerate() {
                        tagged.push((ordinal, local, shard_id, output));
                    }
                }
            }
            tagged
        };

        #[cfg(all(feature = "scheduler", not(target_arch = "wasm32")))]
        let per_shard = if run_parallel {
            if let Some(pool) = crate::executor::worker_pool() {
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
        #[cfg(not(all(feature = "scheduler", not(target_arch = "wasm32"))))]
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

        let mut tagged: Vec<_> = per_shard.into_iter().flatten().collect();
        tagged.sort_by_key(|(ordinal, local, shard, change)| {
            let key_order = if unique_batch || event_orders.is_none() {
                change
                    .atomic_key()
                    .and_then(|key| order.get(key))
                    .copied()
                    .unwrap_or(u64::MAX)
            } else {
                usize::try_from(*ordinal)
                    .ok()
                    .and_then(|index| event_orders.as_ref().and_then(|orders| orders.get(index)))
                    .and_then(|event| change.atomic_key().and_then(|key| event.get(key)))
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

    fn apply_serial_right<Location, RK, RV>(
        runtime: &mut Runtime,
        order: &FxHashMap<K, u64>,
        diff: &MapDiff<RK, RV>,
        maintain: bool,
    ) -> Vec<MapDiff<K, Runtime::Output>>
    where
        RK: Hash + Eq + CellValue,
        RV: CellValue,
        Runtime: RightRoot<Location, K, Input, RK, RV>,
    {
        if !matches!(diff, MapDiff::Batch { .. }) {
            let mut output = runtime.apply_right_root_diff_policy(diff, maintain);
            Self::order_changes(order, &mut output);
            return output;
        }

        let mut leaves = Vec::new();
        diff.visit_leaves(&mut |leaf| leaves.push(leaf));
        let mut output = Vec::new();
        for leaf in leaves {
            let mut phase = Vec::new();
            for change in runtime.apply_right_root_diff_policy(leaf, maintain) {
                change.flatten_into(&mut phase);
            }
            Self::order_changes(order, &mut phase);
            output.extend(phase);
        }
        vec![MapDiff::Batch { changes: output }]
    }

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
        let is_serial = self.storage.is_serial();
        if is_serial && self.shard_count <= 1 {
            return Self::apply_serial_right::<Location, RK, RV>(
                self.storage.serial_mut(),
                &self.key_sequence,
                diff,
                maintain,
            );
        }
        if is_serial && self.shard_count > 1 && diff.work_items() >= self.promotion_work {
            self.promote();
        }
        match &mut self.storage {
            RuntimeStorage::Sharded {
                runtime: shards, ..
            } => {
                let order = &self.key_sequence;
                let preserve_batch = matches!(diff, MapDiff::Batch { .. });
                let mut leaves = Vec::new();
                diff.visit_leaves(&mut |leaf| leaves.push(leaf));
                let mut output = Vec::new();
                // Advance the shared physical index one source member at a time.
                // Every observer shard therefore reads the same snapshot that the
                // sequential runtime used for this phase before the next member.
                for leaf in leaves {
                    let mut phase = Vec::new();
                    for (id, shard) in shards.iter_mut().enumerate() {
                        for change in shard.apply_right_root_diff_policy(leaf, maintain && id == 0)
                        {
                            change.flatten_into(&mut phase);
                        }
                    }
                    Self::order_changes(order, &mut phase);
                    output.extend(phase);
                }
                if preserve_batch {
                    vec![MapDiff::Batch { changes: output }]
                } else {
                    output
                }
            }
            RuntimeStorage::Serial(runtime) => Self::apply_serial_right::<Location, RK, RV>(
                runtime,
                &self.key_sequence,
                diff,
                maintain,
            ),
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
impl<State, K, Output, Tx> InstallRegionRights<State, K, Output, Tx> for JNil
where
    K: Hash + Eq + CellValue,
    Output: CellValue,
    Tx: TransactionPolicy<State>,
{
    fn install(
        self,
        _cx: &mut CompileContext,
        _host: &Arc<RegionHost<State, K, Output, Tx>>,
    ) -> Vec<SubscriptionGuard> {
        Vec::new()
    }
}

impl<Runtime, K, Input, Right, Tail, Location, RK, RV, JK, Policy, Binding, Tx>
    InstallRegionRights<RegionRouter<Runtime, K, Input>, K, Runtime::Output, Tx>
    for RightPlan<Right, Tail, Location, RK, RV, JK, Policy, Binding>
where
    K: Hash + Eq + CellValue,
    Input: CellValue,
    RK: Hash + Eq + CellValue,
    RV: CellValue,
    JK: Hash + Eq + CellValue,
    Policy: IndexPolicy<RK, RV, JK, Binding = Binding>,
    Binding: Clone + Send + Sync + 'static,
    Runtime: RuntimeStages<K, Input>
        + RightRoot<Location, K, Input, RK, RV>
        + EmptyShardRuntime
        + HeadInputSnapshot<K, Input>
        + RuntimeStageCost
        + Send
        + 'static,
    Right: MapQuery<Key = RK, Value = RV>,
    Tail: InstallRegionRights<RegionRouter<Runtime, K, Input>, K, Runtime::Output, Tx>,
    Tx: TransactionPolicy<RegionRouter<Runtime, K, Input>>,
{
    fn install(
        self,
        cx: &mut CompileContext,
        host: &Arc<RegionHost<RegionRouter<Runtime, K, Input>, K, Runtime::Output, Tx>>,
    ) -> Vec<SubscriptionGuard> {
        let callback_binding = self.binding.clone();
        let right_host = Arc::clone(host);
        let callback = move |diff: &MapDiff<RK, RV>| {
            let maintain = Policy::maintains(callback_binding.as_ref());
            right_host.dispatch(|runtime| runtime.apply_right::<Location, RK, RV>(diff, maintain));
        };
        let mut guards = Policy::install(self.right, cx, self.binding, Arc::new(callback));
        guards.extend(self.tail.install(cx, host));
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
    Stages::Rights: InstallRegionRights<
            RegionRouter<Stages::Runtime, K, Input>,
            K,
            Stages::Output,
            FailStopTransaction,
        >,
{
    fn build_into(
        self,
        cx: &mut CompileContext,
        sink: crate::map_query::BoxedMapDiffSink<K, Stages::Output>,
    ) -> Vec<SubscriptionGuard> {
        let (runtime, rights) = self.stages.split(cx);
        let query_poison = cx.query_poison();
        install_region_runtime(
            cx,
            self.left,
            rights,
            RegionRouter::new(runtime),
            RootRegistrationOrder::RightsThenLeft,
            FailStopTransaction::new(query_poison),
            sink,
            RegionRouter::apply_left,
        )
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
    Stages::Rights: InstallRegionRights<
            RegionRouter<Stages::Runtime, K, Input>,
            K,
            Stages::Output,
            FailStopTransaction,
        >,
{
    type Key = K;
    type Value = Stages::Output;
}
