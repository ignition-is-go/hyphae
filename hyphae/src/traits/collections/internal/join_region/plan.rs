use std::{hash::Hash, marker::PhantomData, sync::Arc};

use crate::{
    cell_map::MapDiff,
    map_query::{
        BuildQueryRuntime, MapQuery,
        compiler::CompileContext,
        properties::{ByMapKey, PlanProperties, ZeroOrOne},
    },
    subscription::SubscriptionGuard,
    traits::{
        CellValue, ForeignKeyRelation, IdFor, OptionalRightKey, RequiredRightKey, RightJoinKey,
    },
};

use super::{
    super::join_lifecycle::{
        FailStopTransaction, InstallRegionRights, RegionHost, RootRegistrationOrder,
        TransactionPolicy, install_region_runtime,
    },
    declaration::{
        DirectProject, IndexPolicy, JCons, JNil, JoinStage, LastStage, MapLast, Push,
        ReplaceLastProject, SharedRelationIndex, StageList, StageProject, collect_matches,
        foreign_map_key, map_key,
    },
    router::{Here, RegionRouter, RightRoot, There},
    stage_runtime::{
        EmptyShardRuntime, HeadInputSnapshot, RuntimeStageCost, RuntimeStages, StageRuntimeState,
    },
};

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
