use std::{any::TypeId, hash::Hash, marker::PhantomData};

use crate::{
    map_query::{
        MapQuery, compile_runtime_into,
        compiler::{CompileContext, DeferredPhysical},
        properties::{ByMapKey, Partition, PlanProperties, ZeroOrOne},
    },
    subscription::SubscriptionGuard,
    traits::{CellValue, ForeignKeyRelation, IdFor},
};

use super::super::join_runtime::{RelationIndex, RelationIndexStorage};

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

pub fn collect_matches<K, Input, RK, RV>(
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

pub(super) trait IndexPolicy<RK, RV, JK>: Send + Sync + 'static
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
    pub(super) _types: PhantomData<fn() -> (K, Input, RightKey, RightValue, JoinKey, Output)>,
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
