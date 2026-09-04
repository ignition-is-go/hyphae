use std::{hash::Hash, marker::PhantomData};

use crate::{
    map_query::{
        BuildQueryRuntime, MapQuery,
        properties::{ByMapKey, ExactlyOne, PlanProperties, PreservesMapKey},
    },
    subscription::SubscriptionGuard,
    traits::{
        CellValue, ForeignKeyRelation, IdFor, OptionalRightKey, RequiredRightKey, RightJoinKey,
        collections::internal::{
            join_region::{
                DirectProject, JCons, JNil, JoinRegion, JoinStage, SharedRelationIndex,
                collect_matches,
            },
            join_runtime::install_two_keyed_join_runtime_via_query,
        },
    },
};

use super::{
    DirectJoinProjection, JoinProjection, JoinProjectionProject, JoinedValuesPlan, LeftJoinPlan,
    RelationPlan, TupleJoinProjection,
};
use crate::traits::collections::map_values::MapValuesPlan;

#[allow(private_bounds)]
impl<
    L,
    R1,
    R2,
    LK,
    LV,
    RK1,
    RV1,
    JK1,
    MV1,
    RK2,
    RV2,
    JK2,
    MV2,
    FL1,
    FR1,
    FM1,
    FL2,
    FR2,
    FM2,
    Rel1,
    Rel2,
>
    RelationPlan<
        JoinedValuesPlan<
            RelationPlan<JoinedValuesPlan<L, R1, LK, LV, RK1, RV1, JK1, MV1, FL1, FR1, FM1>, Rel1>,
            R2,
            LK,
            MV1,
            RK2,
            RV2,
            JK2,
            MV2,
            FL2,
            FR2,
            FM2,
        >,
        Rel2,
    >
where
    L: MapQuery<Key = LK, Value = LV> + PreservesMapKey<LK>,
    R1: MapQuery<Key = RK1, Value = RV1>,
    R2: MapQuery<Key = RK2, Value = RV2>,
    LK: Hash + Eq + CellValue,
    LV: CellValue,
    RK1: Hash + Eq + CellValue,
    RV1: CellValue,
    JK1: Hash + Eq + CellValue,
    MV1: CellValue,
    RK2: Hash + Eq + CellValue,
    RV2: CellValue,
    JK2: Hash + Eq + CellValue,
    MV2: CellValue,
    FL1: Fn(&LK, &LV) -> JK1 + Send + Sync + 'static,
    FR1: RightJoinKey<RK1, RV1, JK1>,
    FM1: Fn(&LK, &LV, &[(RK1, RV1)]) -> MV1 + Send + Sync + 'static,
    FL2: Fn(&LK, &MV1) -> JK2 + Send + Sync + 'static,
    FR2: RightJoinKey<RK2, RV2, JK2>,
    FM2: Fn(&LK, &MV1, &[(RK2, RV2)]) -> MV2 + Send + Sync + 'static,
    Rel1: Send + Sync + 'static,
    Rel2: Send + Sync + 'static,
{
    /// Promote three typed joins into an arbitrary-length coordinated region.
    #[allow(clippy::type_complexity)]
    pub fn left_join_fk<Rel3, R3>(
        self,
        right: R3,
    ) -> JoinRegion<
        L,
        JCons<
            JoinStage<
                R1,
                LK,
                LV,
                RK1,
                RV1,
                JK1,
                MV1,
                FL1,
                FR1,
                JoinProjectionProject<DirectJoinProjection<FM1>>,
                SharedRelationIndex<Rel1>,
            >,
            JCons<
                JoinStage<
                    R2,
                    LK,
                    MV1,
                    RK2,
                    RV2,
                    JK2,
                    MV2,
                    FL2,
                    FR2,
                    JoinProjectionProject<DirectJoinProjection<FM2>>,
                    SharedRelationIndex<Rel2>,
                >,
                JCons<
                    JoinStage<
                        R3,
                        LK,
                        MV2,
                        R3::Key,
                        Rel3::Child,
                        LK,
                        (MV2, Vec<Rel3::Child>),
                        fn(&LK, &MV2) -> LK,
                        OptionalRightKey<fn(&R3::Key, &Rel3::Child) -> Option<LK>>,
                        DirectProject<
                            fn(&LK, &MV2, &[(R3::Key, Rel3::Child)]) -> (MV2, Vec<Rel3::Child>),
                        >,
                        SharedRelationIndex<Rel3>,
                    >,
                    JNil,
                >,
            >,
        >,
        LK,
        LV,
    >
    where
        Rel3: ForeignKeyRelation,
        R3: MapQuery<Value = Rel3::Child>,
        Rel3::ForeignKey: IdFor<Rel3::Parent, MapKey = LK>,
    {
        let JoinedValuesPlan {
            join: second_join,
            projection: second_projection,
            ..
        } = self.plan;
        let LeftJoinPlan {
            left: first_relation,
            right: right2,
            left_key: left_key2,
            right_key: right_key2,
            ..
        } = second_join;
        let JoinedValuesPlan {
            join: first_join,
            projection: first_projection,
            ..
        } = first_relation.plan;
        let LeftJoinPlan {
            left,
            right: right1,
            left_key: left_key1,
            right_key: right_key1,
            ..
        } = first_join;
        let third_project: DirectProject<
            fn(&LK, &MV2, &[(R3::Key, Rel3::Child)]) -> (MV2, Vec<Rel3::Child>),
        > = DirectProject(collect_matches::<LK, MV2, R3::Key, Rel3::Child>);
        let first = JoinStage::new(
            right1,
            left_key1,
            right_key1,
            JoinProjectionProject(DirectJoinProjection(first_projection)),
        )
        .with_index_policy(SharedRelationIndex::<Rel1>::new());
        let second = JoinStage::new(
            right2,
            left_key2,
            right_key2,
            JoinProjectionProject(DirectJoinProjection(second_projection)),
        )
        .with_index_policy(SharedRelationIndex::<Rel2>::new());
        let third_left_key: fn(&LK, &MV2) -> LK =
            crate::traits::collections::internal::join_region::map_key::<LK, MV2>;
        let third_right_key: fn(&R3::Key, &Rel3::Child) -> Option<LK> =
            crate::traits::collections::internal::join_region::foreign_map_key::<Rel3, R3::Key, LK>;
        let third = JoinStage::new(
            right,
            third_left_key,
            OptionalRightKey(third_right_key),
            third_project,
        )
        .with_index_policy(SharedRelationIndex::<Rel3>::new());
        JoinRegion::new(
            left,
            JCons {
                head: first,
                tail: JCons {
                    head: second,
                    tail: JCons {
                        head: third,
                        tail: JNil,
                    },
                },
            },
        )
    }
}

/// A statically recognized `left_join -> map_values -> left_join` region.
///
/// The named shape lets installation coordinate all three roots through one
/// state machine instead of installing two independent join runtimes.
pub struct TwoLeftJoinPlan<
    L,
    R1,
    R2,
    LK,
    LV,
    RK1,
    RV1,
    JK1,
    MV,
    RK2,
    RV2,
    JK2,
    FL1,
    FR1,
    FM1,
    FL2,
    FR2,
> where
    L: MapQuery<Key = LK, Value = LV>,
    R1: MapQuery<Key = RK1, Value = RV1>,
    R2: MapQuery<Key = RK2, Value = RV2>,
    LK: Hash + Eq + CellValue,
    LV: CellValue,
    RK1: Hash + Eq + CellValue,
    RV1: CellValue,
    JK1: Hash + Eq + CellValue,
    MV: CellValue,
    RK2: Hash + Eq + CellValue,
    RV2: CellValue,
    JK2: Hash + Eq + CellValue,
    FL1: Fn(&LK, &LV) -> JK1 + Send + Sync + 'static,
    FR1: RightJoinKey<RK1, RV1, JK1>,
    FM1: JoinProjection<LK, LV, RK1, RV1, MV>,
    FL2: Fn(&LK, &MV) -> JK2 + Send + Sync + 'static,
    FR2: RightJoinKey<RK2, RV2, JK2>,
{
    left: L,
    right1: R1,
    right2: R2,
    left_key1: FL1,
    right_key1: FR1,
    map_first: FM1,
    left_key2: FL2,
    right_key2: FR2,
    #[allow(clippy::type_complexity)]
    _types: PhantomData<fn() -> (LK, LV, RK1, RV1, JK1, MV, RK2, RV2, JK2)>,
}

impl<L, R1, R2, LK, LV, RK1, RV1, JK1, MV, RK2, RV2, JK2, FL1, FR1, FM1, FL2, FR2> PlanProperties
    for TwoLeftJoinPlan<
        L,
        R1,
        R2,
        LK,
        LV,
        RK1,
        RV1,
        JK1,
        MV,
        RK2,
        RV2,
        JK2,
        FL1,
        FR1,
        FM1,
        FL2,
        FR2,
    >
where
    L: MapQuery<Key = LK, Value = LV> + PlanProperties,
    R1: MapQuery<Key = RK1, Value = RV1>,
    R2: MapQuery<Key = RK2, Value = RV2>,
    LK: Hash + Eq + CellValue,
    LV: CellValue,
    RK1: Hash + Eq + CellValue,
    RV1: CellValue,
    JK1: Hash + Eq + CellValue,
    MV: CellValue,
    RK2: Hash + Eq + CellValue,
    RV2: CellValue,
    JK2: Hash + Eq + CellValue,
    FL1: Fn(&LK, &LV) -> JK1 + Send + Sync + 'static,
    FR1: RightJoinKey<RK1, RV1, JK1>,
    FM1: JoinProjection<LK, LV, RK1, RV1, MV>,
    FL2: Fn(&LK, &MV) -> JK2 + Send + Sync + 'static,
    FR2: RightJoinKey<RK2, RV2, JK2>,
{
    type Cardinality = ExactlyOne;
    type InputPartition = L::OutputPartition;
    type OutputPartition = ByMapKey<LK>;
}

/// Final projection attached to a coordinated two-left-join region.
pub struct TwoLeftJoinMappedPlan<P, LK, MV, RK2, RV2, OV, F>
where
    P: MapQuery<Key = LK, Value = (MV, Vec<RV2>)>,
    LK: Hash + Eq + CellValue,
    MV: CellValue,
    RK2: Hash + Eq + CellValue,
    RV2: CellValue,
    OV: CellValue,
    F: JoinProjection<LK, MV, RK2, RV2, OV>,
{
    plan: P,
    map_second: F,
    _types: PhantomData<fn() -> (LK, MV, RK2, RV2, OV)>,
}

impl<P, LK, MV, RK2, RV2, OV, F> PlanProperties
    for TwoLeftJoinMappedPlan<P, LK, MV, RK2, RV2, OV, F>
where
    P: MapQuery<Key = LK, Value = (MV, Vec<RV2>)>,
    LK: Hash + Eq + CellValue,
    MV: CellValue,
    RK2: Hash + Eq + CellValue,
    RV2: CellValue,
    OV: CellValue,
    F: JoinProjection<LK, MV, RK2, RV2, OV>,
{
    type Cardinality = ExactlyOne;
    type InputPartition = P::InputPartition;
    type OutputPartition = ByMapKey<LK>;
}

impl<L, R1, R2, LK, LV, RK1, RV1, JK1, MV, RK2, RV2, JK2, FL1, FR1, FM1, FL2, FR2>
    BuildQueryRuntime<LK, (MV, Vec<RV2>)>
    for TwoLeftJoinPlan<
        L,
        R1,
        R2,
        LK,
        LV,
        RK1,
        RV1,
        JK1,
        MV,
        RK2,
        RV2,
        JK2,
        FL1,
        FR1,
        FM1,
        FL2,
        FR2,
    >
where
    L: MapQuery<Key = LK, Value = LV>,
    R1: MapQuery<Key = RK1, Value = RV1>,
    R2: MapQuery<Key = RK2, Value = RV2>,
    LK: Hash + Eq + CellValue,
    LV: CellValue,
    RK1: Hash + Eq + CellValue,
    RV1: CellValue,
    JK1: Hash + Eq + CellValue,
    MV: CellValue,
    RK2: Hash + Eq + CellValue,
    RV2: CellValue,
    JK2: Hash + Eq + CellValue,
    FL1: Fn(&LK, &LV) -> JK1 + Send + Sync + 'static,
    FR1: RightJoinKey<RK1, RV1, JK1>,
    FM1: JoinProjection<LK, LV, RK1, RV1, MV>,
    FL2: Fn(&LK, &MV) -> JK2 + Send + Sync + 'static,
    FR2: RightJoinKey<RK2, RV2, JK2>,
{
    fn build_into(
        self,
        cx: &mut crate::map_query::compiler::CompileContext,
        sink: crate::map_query::BoxedMapDiffSink<LK, (MV, Vec<RV2>)>,
    ) -> Vec<SubscriptionGuard> {
        let map_first = self.map_first;
        install_two_keyed_join_runtime_via_query(
            cx,
            self.left,
            self.right1,
            self.right2,
            self.left_key1,
            self.right_key1,
            move |key, left, rights: &[(RK1, RV1)]| map_first.project(key, left, rights),
            self.left_key2,
            self.right_key2,
            |_key, middle, rights: &[(RK2, RV2)]| {
                (
                    middle.clone(),
                    rights.iter().map(|(_, value)| value.clone()).collect(),
                )
            },
            sink,
        )
    }
}

#[allow(private_bounds)]
impl<L, R1, R2, LK, LV, RK1, RV1, JK1, MV, RK2, RV2, JK2, FL1, FR1, FM1, FL2, FR2> MapQuery
    for TwoLeftJoinPlan<
        L,
        R1,
        R2,
        LK,
        LV,
        RK1,
        RV1,
        JK1,
        MV,
        RK2,
        RV2,
        JK2,
        FL1,
        FR1,
        FM1,
        FL2,
        FR2,
    >
where
    L: MapQuery<Key = LK, Value = LV>,
    R1: MapQuery<Key = RK1, Value = RV1>,
    R2: MapQuery<Key = RK2, Value = RV2>,
    LK: Hash + Eq + CellValue,
    LV: CellValue,
    RK1: Hash + Eq + CellValue,
    RV1: CellValue,
    JK1: Hash + Eq + CellValue,
    MV: CellValue,
    RK2: Hash + Eq + CellValue,
    RV2: CellValue,
    JK2: Hash + Eq + CellValue,
    FL1: Fn(&LK, &LV) -> JK1 + Send + Sync + 'static,
    FR1: RightJoinKey<RK1, RV1, JK1>,
    FM1: JoinProjection<LK, LV, RK1, RV1, MV>,
    FL2: Fn(&LK, &MV) -> JK2 + Send + Sync + 'static,
    FR2: RightJoinKey<RK2, RV2, JK2>,
{
    type Key = LK;
    type Value = (MV, Vec<RV2>);
}

impl<L, R1, R2, LK, LV, RK1, RV1, JK1, MV, RK2, RV2, JK2, OV, FL1, FR1, FM1, FL2, FR2, FM2>
    BuildQueryRuntime<LK, OV>
    for TwoLeftJoinMappedPlan<
        TwoLeftJoinPlan<
            L,
            R1,
            R2,
            LK,
            LV,
            RK1,
            RV1,
            JK1,
            MV,
            RK2,
            RV2,
            JK2,
            FL1,
            FR1,
            FM1,
            FL2,
            FR2,
        >,
        LK,
        MV,
        RK2,
        RV2,
        OV,
        FM2,
    >
where
    L: MapQuery<Key = LK, Value = LV>,
    R1: MapQuery<Key = RK1, Value = RV1>,
    R2: MapQuery<Key = RK2, Value = RV2>,
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
    FR1: RightJoinKey<RK1, RV1, JK1>,
    FM1: JoinProjection<LK, LV, RK1, RV1, MV>,
    FL2: Fn(&LK, &MV) -> JK2 + Send + Sync + 'static,
    FR2: RightJoinKey<RK2, RV2, JK2>,
    FM2: JoinProjection<LK, MV, RK2, RV2, OV>,
{
    fn build_into(
        self,
        cx: &mut crate::map_query::compiler::CompileContext,
        sink: crate::map_query::BoxedMapDiffSink<LK, OV>,
    ) -> Vec<SubscriptionGuard> {
        let plan = self.plan;
        let map_first = plan.map_first;
        let map_second = self.map_second;
        install_two_keyed_join_runtime_via_query(
            cx,
            plan.left,
            plan.right1,
            plan.right2,
            plan.left_key1,
            plan.right_key1,
            move |key, left, rights: &[(RK1, RV1)]| map_first.project(key, left, rights),
            plan.left_key2,
            plan.right_key2,
            move |key, middle, rights: &[(RK2, RV2)]| map_second.project(key, middle, rights),
            sink,
        )
    }
}

#[allow(private_bounds)]
impl<P, LK, MV, RK2, RV2, OV, F> MapQuery for TwoLeftJoinMappedPlan<P, LK, MV, RK2, RV2, OV, F>
where
    P: MapQuery<Key = LK, Value = (MV, Vec<RV2>)>,
    LK: Hash + Eq + CellValue,
    MV: CellValue,
    RK2: Hash + Eq + CellValue,
    RV2: CellValue,
    OV: CellValue,
    F: JoinProjection<LK, MV, RK2, RV2, OV>,
    Self: BuildQueryRuntime<LK, OV>,
{
    type Key = LK;
    type Value = OV;
}

impl<L, R1, R2, LK, LV, RK1, RV1, JK1, MV, RK2, RV2, JK2, FL1, FR1, FM1, FL2, FR2>
    TwoLeftJoinPlan<L, R1, R2, LK, LV, RK1, RV1, JK1, MV, RK2, RV2, JK2, FL1, FR1, FM1, FL2, FR2>
where
    L: MapQuery<Key = LK, Value = LV>,
    R1: MapQuery<Key = RK1, Value = RV1>,
    R2: MapQuery<Key = RK2, Value = RV2>,
    LK: Hash + Eq + CellValue,
    LV: CellValue,
    RK1: Hash + Eq + CellValue,
    RV1: CellValue,
    JK1: Hash + Eq + CellValue,
    MV: CellValue,
    RK2: Hash + Eq + CellValue,
    RV2: CellValue,
    JK2: Hash + Eq + CellValue,
    FL1: Fn(&LK, &LV) -> JK1 + Send + Sync + 'static,
    FR1: RightJoinKey<RK1, RV1, JK1>,
    FM1: JoinProjection<LK, LV, RK1, RV1, MV>,
    FL2: Fn(&LK, &MV) -> JK2 + Send + Sync + 'static,
    FR2: RightJoinKey<RK2, RV2, JK2>,
{
    /// Attach the final key-preserving projection without breaking the
    /// coordinated physical region apart.
    pub fn map_values<OV, F>(
        self,
        f: F,
    ) -> TwoLeftJoinMappedPlan<Self, LK, MV, RK2, RV2, OV, TupleJoinProjection<F>>
    where
        OV: CellValue,
        F: Fn(&LK, &(MV, Vec<RV2>)) -> OV + Send + Sync + 'static,
    {
        TwoLeftJoinMappedPlan {
            plan: self,
            map_second: TupleJoinProjection(f),
            _types: PhantomData,
        }
    }

    /// Project the second join directly without constructing `(middle, Vec)`.
    pub fn map_joined_values<OV, F>(
        self,
        f: F,
    ) -> TwoLeftJoinMappedPlan<Self, LK, MV, RK2, RV2, OV, DirectJoinProjection<F>>
    where
        OV: CellValue,
        F: Fn(&LK, &MV, &[(RK2, RV2)]) -> OV + Send + Sync + 'static,
    {
        TwoLeftJoinMappedPlan {
            plan: self,
            map_second: DirectJoinProjection(f),
            _types: PhantomData,
        }
    }
}

#[allow(private_bounds)]
impl<L, R1, R2, LK, LV, RK1, RV1, JK1, MV, RK2, RV2, JK2, OV, FL1, FR1, FM1, FL2, FR2, FM2>
    TwoLeftJoinMappedPlan<
        TwoLeftJoinPlan<
            L,
            R1,
            R2,
            LK,
            LV,
            RK1,
            RV1,
            JK1,
            MV,
            RK2,
            RV2,
            JK2,
            FL1,
            FR1,
            FM1,
            FL2,
            FR2,
        >,
        LK,
        MV,
        RK2,
        RV2,
        OV,
        FM2,
    >
where
    L: MapQuery<Key = LK, Value = LV> + PreservesMapKey<LK>,
    R1: MapQuery<Key = RK1, Value = RV1>,
    R2: MapQuery<Key = RK2, Value = RV2>,
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
    FR1: RightJoinKey<RK1, RV1, JK1>,
    FM1: JoinProjection<LK, LV, RK1, RV1, MV>,
    FL2: Fn(&LK, &MV) -> JK2 + Send + Sync + 'static,
    FR2: RightJoinKey<RK2, RV2, JK2>,
    FM2: JoinProjection<LK, MV, RK2, RV2, OV>,
{
    /// Promote a proven two-stage region and append its third left join.
    #[allow(clippy::type_complexity)]
    pub fn left_join_by<R3, RK3, RV3, JK3, FL3, FR3>(
        self,
        right: R3,
        left_key: FL3,
        right_key: FR3,
    ) -> JoinRegion<
        L,
        JCons<
            JoinStage<R1, LK, LV, RK1, RV1, JK1, MV, FL1, FR1, JoinProjectionProject<FM1>>,
            JCons<
                JoinStage<R2, LK, MV, RK2, RV2, JK2, OV, FL2, FR2, JoinProjectionProject<FM2>>,
                JCons<
                    JoinStage<
                        R3,
                        LK,
                        OV,
                        RK3,
                        RV3,
                        JK3,
                        (OV, Vec<RV3>),
                        FL3,
                        RequiredRightKey<FR3>,
                        DirectProject<fn(&LK, &OV, &[(RK3, RV3)]) -> (OV, Vec<RV3>)>,
                    >,
                    JNil,
                >,
            >,
        >,
        LK,
        LV,
    >
    where
        R3: MapQuery<Key = RK3, Value = RV3>,
        RK3: Hash + Eq + CellValue,
        RV3: CellValue,
        JK3: Hash + Eq + CellValue,
        FL3: Fn(&LK, &OV) -> JK3 + Send + Sync + 'static,
        FR3: Fn(&RK3, &RV3) -> JK3 + Send + Sync + 'static,
    {
        let Self {
            plan, map_second, ..
        } = self;
        let TwoLeftJoinPlan {
            left,
            right1,
            right2,
            left_key1,
            right_key1,
            map_first,
            left_key2,
            right_key2,
            ..
        } = plan;
        let third_project: DirectProject<fn(&LK, &OV, &[(RK3, RV3)]) -> (OV, Vec<RV3>)> =
            DirectProject(collect_matches::<LK, OV, RK3, RV3>);
        JoinRegion::new(
            left,
            JCons {
                head: JoinStage::new(
                    right1,
                    left_key1,
                    right_key1,
                    JoinProjectionProject(map_first),
                ),
                tail: JCons {
                    head: JoinStage::new(
                        right2,
                        left_key2,
                        right_key2,
                        JoinProjectionProject(map_second),
                    ),
                    tail: JCons {
                        head: JoinStage::new(
                            right,
                            left_key,
                            RequiredRightKey(right_key),
                            third_project,
                        ),
                        tail: JNil,
                    },
                },
            },
        )
    }
}

#[allow(private_bounds)]
impl<L, R1, LK, LV, RK1, RV1, JK1, MV, FL1, FR1, FM1>
    MapValuesPlan<LeftJoinPlan<L, R1, LK, LV, RK1, RV1, JK1, FL1, FR1>, LK, (LV, Vec<RV1>), MV, FM1>
where
    L: MapQuery<Key = LK, Value = LV> + PreservesMapKey<LK>,
    R1: MapQuery<Key = RK1, Value = RV1>,
    LK: Hash + Eq + CellValue,
    LV: CellValue,
    RK1: Hash + Eq + CellValue,
    RV1: CellValue,
    JK1: Hash + Eq + CellValue,
    MV: CellValue,
    FL1: Fn(&LK, &LV) -> JK1 + Send + Sync + 'static,
    FR1: RightJoinKey<RK1, RV1, JK1>,
    FM1: Fn(&LK, &(LV, Vec<RV1>)) -> MV + Send + Sync + 'static,
{
    /// Extend a recognized join/projection region with a second left join.
    pub fn left_join_by<R2, RK2, RV2, JK2, FL2, FR2>(
        self,
        right: R2,
        left_key: FL2,
        right_key: FR2,
    ) -> TwoLeftJoinPlan<
        L,
        R1,
        R2,
        LK,
        LV,
        RK1,
        RV1,
        JK1,
        MV,
        RK2,
        RV2,
        JK2,
        FL1,
        FR1,
        TupleJoinProjection<FM1>,
        FL2,
        RequiredRightKey<FR2>,
    >
    where
        R2: MapQuery<Key = RK2, Value = RV2>,
        RK2: Hash + Eq + CellValue,
        RV2: CellValue,
        JK2: Hash + Eq + CellValue,
        FL2: Fn(&LK, &MV) -> JK2 + Send + Sync + 'static,
        FR2: Fn(&RK2, &RV2) -> JK2 + Send + Sync + 'static,
    {
        let LeftJoinPlan {
            left,
            right: right1,
            left_key: left_key1,
            right_key: right_key1,
            ..
        } = self.source;
        TwoLeftJoinPlan {
            left,
            right1,
            right2: right,
            left_key1,
            right_key1,
            map_first: TupleJoinProjection(self.f),
            left_key2: left_key,
            right_key2: RequiredRightKey(right_key),
            _types: PhantomData,
        }
    }
}

#[allow(private_bounds)]
impl<L, R1, LK, LV, RK1, RV1, JK1, MV, FL1, FR1, FM1>
    JoinedValuesPlan<L, R1, LK, LV, RK1, RV1, JK1, MV, FL1, FR1, FM1>
where
    L: MapQuery<Key = LK, Value = LV> + PreservesMapKey<LK>,
    R1: MapQuery<Key = RK1, Value = RV1>,
    LK: Hash + Eq + CellValue,
    LV: CellValue,
    RK1: Hash + Eq + CellValue,
    RV1: CellValue,
    JK1: Hash + Eq + CellValue,
    MV: CellValue,
    FL1: Fn(&LK, &LV) -> JK1 + Send + Sync + 'static,
    FR1: RightJoinKey<RK1, RV1, JK1>,
    FM1: Fn(&LK, &LV, &[(RK1, RV1)]) -> MV + Send + Sync + 'static,
{
    /// Extend a direct joined projection with a coordinated second left join.
    pub fn left_join_by<R2, RK2, RV2, JK2, FL2, FR2>(
        self,
        right: R2,
        left_key: FL2,
        right_key: FR2,
    ) -> TwoLeftJoinPlan<
        L,
        R1,
        R2,
        LK,
        LV,
        RK1,
        RV1,
        JK1,
        MV,
        RK2,
        RV2,
        JK2,
        FL1,
        FR1,
        DirectJoinProjection<FM1>,
        FL2,
        RequiredRightKey<FR2>,
    >
    where
        R2: MapQuery<Key = RK2, Value = RV2>,
        RK2: Hash + Eq + CellValue,
        RV2: CellValue,
        JK2: Hash + Eq + CellValue,
        FL2: Fn(&LK, &MV) -> JK2 + Send + Sync + 'static,
        FR2: Fn(&RK2, &RV2) -> JK2 + Send + Sync + 'static,
    {
        let LeftJoinPlan {
            left,
            right: right1,
            left_key: left_key1,
            right_key: right_key1,
            ..
        } = self.join;
        TwoLeftJoinPlan {
            left,
            right1,
            right2: right,
            left_key1,
            right_key1,
            map_first: DirectJoinProjection(self.projection),
            left_key2: left_key,
            right_key2: RequiredRightKey(right_key),
            _types: PhantomData,
        }
    }
}
