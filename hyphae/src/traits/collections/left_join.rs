//! Left-join plan nodes implementing [`MapQuery`].
//!
//! `left_join`, `left_join_fk`, and `left_join_by` build uncompiled plan nodes
//! that compose with other [`MapQuery`] operators. Call
//! [`MapQuery::materialize`] to compile a plan into a subscribable
//! [`CellMap`](crate::CellMap).

use std::{hash::Hash, marker::PhantomData};

use crate::{
    map_query::{
        BuildQueryRuntime, MapQuery,
        properties::{ByMapKey, ByRelation, ExactlyOne, PlanProperties, PreservesMapKey},
    },
    subscription::SubscriptionGuard,
    traits::{
        CellValue, ForeignKeyRelation, IdFor, OptionalRightKey, RequiredRightKey, RightJoinKey,
        collections::internal::{
            join_region::{
                DirectProject, JCons, JNil, JoinRegion, JoinStage, SharedRelationIndex,
                StageProject, collect_matches,
            },
            join_runtime::{
                install_keyed_join_runtime_via_query, install_two_keyed_join_runtime_via_query,
            },
        },
    },
};

use super::map_values::MapValuesPlan;

/// A query node carrying the semantic identity of a typed relationship.
#[doc(hidden)]
pub struct RelationPlan<P, Rel> {
    plan: P,
    _relation: PhantomData<fn() -> Rel>,
}

impl<P, Rel> RelationPlan<P, Rel> {
    pub(crate) const fn new(plan: P) -> Self {
        Self {
            plan,
            _relation: PhantomData,
        }
    }
}

impl<P, Rel> PlanProperties for RelationPlan<P, Rel>
where
    P: PlanProperties,
    Rel: Send + Sync + 'static,
{
    type Cardinality = P::Cardinality;
    type InputPartition = P::InputPartition;
    type OutputPartition = ByRelation<Rel>;
}

impl<K, V, P, Rel> BuildQueryRuntime<K, V> for RelationPlan<P, Rel>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
    P: MapQuery<Key = K, Value = V>,
    Rel: Send + Sync + 'static,
{
    fn build_into(
        self,
        cx: &mut crate::map_query::compiler::CompileContext,
        sink: crate::map_query::BoxedMapDiffSink<K, V>,
    ) -> Vec<SubscriptionGuard> {
        cx.with_relation_hint::<Rel, _>(|cx| {
            crate::map_query::compile_runtime_into(self.plan, cx, sink)
        })
    }
}

#[allow(private_bounds)]
impl<K, V, P, Rel> MapQuery for RelationPlan<P, Rel>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
    P: MapQuery<Key = K, Value = V>,
    Rel: Send + Sync + 'static,
{
    type Key = K;
    type Value = V;
}

/// Plan node for [`LeftJoinExt::left_join`], [`LeftJoinExt::left_join_fk`],
/// and [`LeftJoinExt::left_join_by`].
///
/// Every left row produces exactly one output row, keyed by the left key.
/// Right matches are collected into a `Vec`; an empty `Vec` means no matching
/// right rows were found. Output value type is `(LV, Vec<RV>)`.
///
/// Not [`Clone`]: cloning a plan would silently duplicate join work; share by
/// materializing once.
pub struct LeftJoinPlan<L, R, LK, LV, RK, RV, JK, FL, FR>
where
    L: MapQuery<Key = LK, Value = LV>,
    R: MapQuery<Key = RK, Value = RV>,
    LK: Hash + Eq + CellValue,
    LV: CellValue,
    RK: Hash + Eq + CellValue,
    RV: CellValue,
    JK: Hash + Eq + CellValue,
    FL: Fn(&LK, &LV) -> JK + Send + Sync + 'static,
    FR: RightJoinKey<RK, RV, JK>,
{
    pub(crate) left: L,
    pub(crate) right: R,
    pub(crate) left_key: FL,
    pub(crate) right_key: FR,
    #[allow(clippy::type_complexity)]
    pub(crate) _types: PhantomData<fn() -> (LK, LV, RK, RV, JK)>,
}

impl<L, R, LK, LV, RK, RV, JK, FL, FR> PlanProperties
    for LeftJoinPlan<L, R, LK, LV, RK, RV, JK, FL, FR>
where
    L: MapQuery<Key = LK, Value = LV> + PlanProperties,
    R: MapQuery<Key = RK, Value = RV>,
    LK: Hash + Eq + CellValue,
    LV: CellValue,
    RK: Hash + Eq + CellValue,
    RV: CellValue,
    JK: Hash + Eq + CellValue,
    FL: Fn(&LK, &LV) -> JK + Send + Sync + 'static,
    FR: RightJoinKey<RK, RV, JK>,
{
    type Cardinality = ExactlyOne;
    type InputPartition = L::OutputPartition;
    type OutputPartition = ByMapKey<LK>;
}

/// Internal projection protocol carried by statically recognized join plans.
mod join_projection_private {
    pub trait Sealed {}
}

/// Internal projection protocol carried by statically recognized join plans.
#[doc(hidden)]
#[allow(private_bounds)]
pub trait JoinProjection<LK, LV, RK, RV, OV>:
    join_projection_private::Sealed + Send + Sync + 'static
{
    fn project(&self, key: &LK, left: &LV, rights: &[(RK, RV)]) -> OV;
}

/// Adapts a recognized public join projection to a join-region stage.
#[doc(hidden)]
pub struct JoinProjectionProject<P>(pub P);

impl<LK, LV, RK, RV, OV, P> StageProject<LK, LV, RK, RV, OV> for JoinProjectionProject<P>
where
    P: JoinProjection<LK, LV, RK, RV, OV>,
{
    fn project(&self, key: &LK, left: &LV, rights: &[(RK, RV)]) -> Option<OV> {
        Some(self.0.project(key, left, rights))
    }
}

/// Adapter for the ordinary `map_values(|key, (left, rights)| ...)` surface.
#[doc(hidden)]
pub struct TupleJoinProjection<F>(F);

impl<F> join_projection_private::Sealed for TupleJoinProjection<F> {}

impl<LK, LV, RK, RV, OV, F> JoinProjection<LK, LV, RK, RV, OV> for TupleJoinProjection<F>
where
    LV: Clone,
    RV: Clone,
    F: Fn(&LK, &(LV, Vec<RV>)) -> OV + Send + Sync + 'static,
{
    fn project(&self, key: &LK, left: &LV, rights: &[(RK, RV)]) -> OV {
        let joined = (
            left.clone(),
            rights.iter().map(|(_, value)| value.clone()).collect(),
        );
        (self.0)(key, &joined)
    }
}

/// Adapter for `map_joined_values`, which avoids materializing a joined tuple.
#[doc(hidden)]
pub struct DirectJoinProjection<F>(F);

impl<F> join_projection_private::Sealed for DirectJoinProjection<F> {}

impl<LK, LV, RK, RV, OV, F> JoinProjection<LK, LV, RK, RV, OV> for DirectJoinProjection<F>
where
    F: Fn(&LK, &LV, &[(RK, RV)]) -> OV + Send + Sync + 'static,
{
    fn project(&self, key: &LK, left: &LV, rights: &[(RK, RV)]) -> OV {
        (self.0)(key, left, rights)
    }
}

/// A left join whose output is projected directly from the indexed matches.
pub struct JoinedValuesPlan<L, R, LK, LV, RK, RV, JK, OV, FL, FR, F>
where
    L: MapQuery<Key = LK, Value = LV>,
    R: MapQuery<Key = RK, Value = RV>,
    LK: Hash + Eq + CellValue,
    LV: CellValue,
    RK: Hash + Eq + CellValue,
    RV: CellValue,
    JK: Hash + Eq + CellValue,
    OV: CellValue,
    FL: Fn(&LK, &LV) -> JK + Send + Sync + 'static,
    FR: RightJoinKey<RK, RV, JK>,
    F: Fn(&LK, &LV, &[(RK, RV)]) -> OV + Send + Sync + 'static,
{
    join: LeftJoinPlan<L, R, LK, LV, RK, RV, JK, FL, FR>,
    projection: F,
    _output: PhantomData<fn() -> OV>,
}

impl<L, R, LK, LV, RK, RV, JK, OV, FL, FR, F> PlanProperties
    for JoinedValuesPlan<L, R, LK, LV, RK, RV, JK, OV, FL, FR, F>
where
    L: MapQuery<Key = LK, Value = LV> + PlanProperties,
    R: MapQuery<Key = RK, Value = RV>,
    LK: Hash + Eq + CellValue,
    LV: CellValue,
    RK: Hash + Eq + CellValue,
    RV: CellValue,
    JK: Hash + Eq + CellValue,
    OV: CellValue,
    FL: Fn(&LK, &LV) -> JK + Send + Sync + 'static,
    FR: RightJoinKey<RK, RV, JK>,
    F: Fn(&LK, &LV, &[(RK, RV)]) -> OV + Send + Sync + 'static,
{
    type Cardinality = ExactlyOne;
    type InputPartition = L::OutputPartition;
    type OutputPartition = ByMapKey<LK>;
}

impl<L, R, LK, LV, RK, RV, JK, OV, FL, FR, F> BuildQueryRuntime<LK, OV>
    for JoinedValuesPlan<L, R, LK, LV, RK, RV, JK, OV, FL, FR, F>
where
    L: MapQuery<Key = LK, Value = LV>,
    R: MapQuery<Key = RK, Value = RV>,
    LK: Hash + Eq + CellValue,
    LV: CellValue,
    RK: Hash + Eq + CellValue,
    RV: CellValue,
    JK: Hash + Eq + CellValue,
    OV: CellValue,
    FL: Fn(&LK, &LV) -> JK + Send + Sync + 'static,
    FR: RightJoinKey<RK, RV, JK>,
    F: Fn(&LK, &LV, &[(RK, RV)]) -> OV + Send + Sync + 'static,
{
    fn build_into(
        self,
        cx: &mut crate::map_query::compiler::CompileContext,
        sink: crate::map_query::BoxedMapDiffSink<LK, OV>,
    ) -> Vec<SubscriptionGuard> {
        install_keyed_join_runtime_via_query(
            cx,
            self.join.left,
            self.join.right,
            self.join.left_key,
            self.join.right_key,
            move |key, left, rights| Some((self.projection)(key, left, rights)),
            sink,
        )
    }
}

#[allow(private_bounds)]
impl<L, R, LK, LV, RK, RV, JK, OV, FL, FR, F> MapQuery
    for JoinedValuesPlan<L, R, LK, LV, RK, RV, JK, OV, FL, FR, F>
where
    L: MapQuery<Key = LK, Value = LV>,
    R: MapQuery<Key = RK, Value = RV>,
    LK: Hash + Eq + CellValue,
    LV: CellValue,
    RK: Hash + Eq + CellValue,
    RV: CellValue,
    JK: Hash + Eq + CellValue,
    OV: CellValue,
    FL: Fn(&LK, &LV) -> JK + Send + Sync + 'static,
    FR: RightJoinKey<RK, RV, JK>,
    F: Fn(&LK, &LV, &[(RK, RV)]) -> OV + Send + Sync + 'static,
{
    type Key = LK;
    type Value = OV;
}

impl<L, R, LK, LV, RK, RV, JK, FL, FR> BuildQueryRuntime<LK, (LV, Vec<RV>)>
    for LeftJoinPlan<L, R, LK, LV, RK, RV, JK, FL, FR>
where
    L: MapQuery<Key = LK, Value = LV>,
    R: MapQuery<Key = RK, Value = RV>,
    LK: Hash + Eq + CellValue,
    LV: CellValue,
    RK: Hash + Eq + CellValue,
    RV: CellValue,
    JK: Hash + Eq + CellValue,
    FL: Fn(&LK, &LV) -> JK + Send + Sync + 'static,
    FR: RightJoinKey<RK, RV, JK>,
{
    fn build_into(
        self,
        cx: &mut crate::map_query::compiler::CompileContext,
        sink: crate::map_query::BoxedMapDiffSink<LK, (LV, Vec<RV>)>,
    ) -> Vec<SubscriptionGuard> {
        install_keyed_join_runtime_via_query::<LK, LV, RK, RV, JK, (LV, Vec<RV>), _, _, _, _, _>(
            cx,
            self.left,
            self.right,
            self.left_key,
            self.right_key,
            |_left_k: &LK, left_v: &LV, rights: &[(RK, RV)]| {
                let right_values: Vec<RV> = rights.iter().map(|(_, rv)| rv.clone()).collect();
                Some((left_v.clone(), right_values))
            },
            sink,
        )
    }
}

#[allow(private_bounds)]
impl<L, R, LK, LV, RK, RV, JK, FL, FR> MapQuery for LeftJoinPlan<L, R, LK, LV, RK, RV, JK, FL, FR>
where
    L: MapQuery<Key = LK, Value = LV>,
    R: MapQuery<Key = RK, Value = RV>,
    LK: Hash + Eq + CellValue,
    LV: CellValue,
    RK: Hash + Eq + CellValue,
    RV: CellValue,
    JK: Hash + Eq + CellValue,
    FL: Fn(&LK, &LV) -> JK + Send + Sync + 'static,
    FR: RightJoinKey<RK, RV, JK>,
{
    type Key = LK;
    type Value = (LV, Vec<RV>);
}

impl<L, R, LK, LV, RK, RV, JK, FL, FR> LeftJoinPlan<L, R, LK, LV, RK, RV, JK, FL, FR>
where
    L: MapQuery<Key = LK, Value = LV>,
    R: MapQuery<Key = RK, Value = RV>,
    LK: Hash + Eq + CellValue,
    LV: CellValue,
    RK: Hash + Eq + CellValue,
    RV: CellValue,
    JK: Hash + Eq + CellValue,
    FL: Fn(&LK, &LV) -> JK + Send + Sync + 'static,
    FR: RightJoinKey<RK, RV, JK>,
{
    /// Project a joined row directly from the left value and indexed matches.
    ///
    /// Unlike `map_values`, this does not first clone right values into an
    /// intermediate `Vec`; use it when the joined tuple is not itself needed.
    /// The closure follows [`MapQuery`]'s purity/invocation contract.
    ///
    /// Keeping this direct fluent projection lets the compiler recognize a
    /// coordinated chain: one/two joins use specialized shapes, the third
    /// recognized join promotes to a concrete `JoinRegion`, and later joins
    /// extend its typed stage list. Rekeys and unsupported shapes are region
    /// boundaries.
    pub fn map_joined_values<OV, F>(
        self,
        projection: F,
    ) -> JoinedValuesPlan<L, R, LK, LV, RK, RV, JK, OV, FL, FR, F>
    where
        OV: CellValue,
        F: Fn(&LK, &LV, &[(RK, RV)]) -> OV + Send + Sync + 'static,
    {
        JoinedValuesPlan {
            join: self,
            projection,
            _output: PhantomData,
        }
    }
}

impl<L, R, LK, LV, RK, RV, JK, FL, FR, Rel>
    RelationPlan<LeftJoinPlan<L, R, LK, LV, RK, RV, JK, FL, FR>, Rel>
where
    L: MapQuery<Key = LK, Value = LV>,
    R: MapQuery<Key = RK, Value = RV>,
    LK: Hash + Eq + CellValue,
    LV: CellValue,
    RK: Hash + Eq + CellValue,
    RV: CellValue,
    JK: Hash + Eq + CellValue,
    FL: Fn(&LK, &LV) -> JK + Send + Sync + 'static,
    FR: RightJoinKey<RK, RV, JK>,
    Rel: Send + Sync + 'static,
{
    /// Project a relationship-typed join without erasing its marker.
    pub fn map_joined_values<OV, F>(
        self,
        projection: F,
    ) -> RelationPlan<JoinedValuesPlan<L, R, LK, LV, RK, RV, JK, OV, FL, FR, F>, Rel>
    where
        OV: CellValue,
        F: Fn(&LK, &LV, &[(RK, RV)]) -> OV + Send + Sync + 'static,
    {
        RelationPlan {
            plan: self.plan.map_joined_values(projection),
            _relation: PhantomData,
        }
    }
}

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

/// Left-join operators returning [`MapQuery`] plan nodes.
///
/// All three methods consume `self` and return uncompiled plan nodes; call
/// [`MapQuery::materialize`] on the result to obtain a subscribable
/// [`CellMap`](crate::CellMap).
pub trait LeftJoinExt<K, V>: MapQuery<Key = K, Value = V>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    /// Left join on equal map keys.
    ///
    /// Every left row produces exactly one output row, keyed by the shared
    /// key. Right matches are collected into a `Vec`; an empty `Vec` means no
    /// matching right rows were found.
    #[allow(clippy::type_complexity)]
    fn left_join<R, RV>(
        self,
        right: R,
    ) -> LeftJoinPlan<
        Self,
        R,
        K,
        V,
        K,
        RV,
        K,
        impl Fn(&K, &V) -> K + Send + Sync + 'static,
        RequiredRightKey<impl Fn(&K, &RV) -> K + Send + Sync + 'static>,
    >
    where
        R: MapQuery<Key = K, Value = RV>,
        RV: CellValue,
    {
        LeftJoinPlan {
            left: self,
            right,
            left_key: |k: &K, _: &V| k.clone(),
            right_key: RequiredRightKey(|k: &K, _: &RV| k.clone()),
            _types: PhantomData,
        }
    }

    /// Left join using foreign key relationship.
    ///
    /// Joins on the left map key matching the right value's foreign key.
    /// Every left row produces exactly one output row, keyed by the left key.
    /// Right matches are collected into a `Vec`; an empty `Vec` means no
    /// matching right rows were found.
    ///
    /// `Rel` is the relationship's semantic, partition, and reusable-index
    /// identity. `None` from its extractor denotes an absent optional right
    /// relationship and is omitted from the index. The key is converted into
    /// `IdFor<Rel::Parent>::MapKey`; it is independent of the left payload.
    #[allow(clippy::type_complexity)]
    fn left_join_fk<Rel, R>(
        self,
        right: R,
    ) -> RelationPlan<
        LeftJoinPlan<
            Self,
            R,
            K,
            V,
            R::Key,
            Rel::Child,
            K,
            impl Fn(&K, &V) -> K + Send + Sync + 'static,
            OptionalRightKey<impl Fn(&R::Key, &Rel::Child) -> Option<K> + Send + Sync + 'static>,
        >,
        Rel,
    >
    where
        Rel: ForeignKeyRelation,
        R: MapQuery<Value = Rel::Child>,
        Rel::ForeignKey: IdFor<Rel::Parent, MapKey = K>,
    {
        RelationPlan {
            plan: LeftJoinPlan {
                left: self,
                right,
                left_key: |k: &K, _: &V| k.clone(),
                right_key: OptionalRightKey(|_: &R::Key, rv: &Rel::Child| {
                    Rel::foreign_key(rv).map(|foreign_key| foreign_key.map_key())
                }),
                _types: PhantomData,
            },
            _relation: PhantomData,
        }
    }

    /// Left join using explicit key extractors.
    ///
    /// `left_key` and `right_key` extract the join key from each side.
    /// This is an ad hoc escape hatch and carries no reusable typed relation
    /// identity; prefer the typed FK form for schema-owned relationships.
    /// Every left row produces exactly one output row, keyed by the left key.
    /// Right matches are collected into a `Vec`; an empty `Vec` means no
    /// matching right rows were found.
    fn left_join_by<R, RK, RV, JK, FL, FR>(
        self,
        right: R,
        left_key: FL,
        right_key: FR,
    ) -> LeftJoinPlan<Self, R, K, V, RK, RV, JK, FL, RequiredRightKey<FR>>
    where
        R: MapQuery<Key = RK, Value = RV>,
        RK: Hash + Eq + CellValue,
        RV: CellValue,
        JK: Hash + Eq + CellValue,
        FL: Fn(&K, &V) -> JK + Send + Sync + 'static,
        FR: Fn(&RK, &RV) -> JK + Send + Sync + 'static,
    {
        LeftJoinPlan {
            left: self,
            right,
            left_key,
            right_key: RequiredRightKey(right_key),
            _types: PhantomData,
        }
    }
}

impl<K, V, M> LeftJoinExt<K, V> for M
where
    K: Hash + Eq + CellValue,
    V: CellValue,
    M: MapQuery<Key = K, Value = V>,
{
}

#[cfg(test)]
mod tests;
