//! Left-join plan nodes implementing [`MapQuery`].
//!
//! `left_join`, `left_join_fk`, and `left_join_by` build uncompiled plan nodes
//! that compose with other [`MapQuery`] operators. Call
//! [`MapQuery::materialize`] to compile a plan into a subscribable
//! [`CellMap`](crate::CellMap).

use std::{hash::Hash, marker::PhantomData};

use crate::{
    map_query::{
        BuildQueryRuntime, MapDiffSink, MapQuery,
        properties::{ByMapKey, ByRelation, ExactlyOne, PlanProperties, PreservesMapKey},
    },
    subscription::SubscriptionGuard,
    traits::{
        CellValue, ForeignKeyRelation, IdFor, OptionalRightKey, RequiredRightKey, RightJoinKey,
        collections::internal::{
            join_region::{
                DirectProject, JCons, JNil, JoinRegion, JoinStage, StageProject, collect_matches,
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
    fn build_into<Sink>(
        self,
        cx: &mut crate::map_query::compiler::CompileContext,
        sink: Sink,
    ) -> Vec<SubscriptionGuard>
    where
        Sink: MapDiffSink<K, V>,
    {
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
    fn build_into<Sink>(
        self,
        cx: &mut crate::map_query::compiler::CompileContext,
        sink: Sink,
    ) -> Vec<SubscriptionGuard>
    where
        Sink: MapDiffSink<LK, OV>,
    {
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
    fn build_into<Sink>(
        self,
        cx: &mut crate::map_query::compiler::CompileContext,
        sink: Sink,
    ) -> Vec<SubscriptionGuard>
    where
        Sink: MapDiffSink<LK, (LV, Vec<RV>)>,
    {
        install_keyed_join_runtime_via_query::<LK, LV, RK, RV, JK, (LV, Vec<RV>), _, _, _, _, _, _>(
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
    fn build_into<Sink>(
        self,
        cx: &mut crate::map_query::compiler::CompileContext,
        sink: Sink,
    ) -> Vec<SubscriptionGuard>
    where
        Sink: MapDiffSink<LK, (MV, Vec<RV2>)>,
    {
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
    fn build_into<Sink>(
        self,
        cx: &mut crate::map_query::compiler::CompileContext,
        sink: Sink,
    ) -> Vec<SubscriptionGuard>
    where
        Sink: MapDiffSink<LK, OV>,
    {
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
mod tests {
    use std::sync::mpsc;

    use super::*;
    use crate::{
        CellMap, MapDiff, MapValuesExt, Materialize,
        traits::{ForeignKeyRelation, Gettable, IdFor, IdType},
    };

    #[test]
    fn left_join_keeps_unmatched_left_rows() {
        let left = CellMap::<String, i32>::new();
        let right = CellMap::<String, i32>::new();
        let joined = left.clone().left_join(right).materialize();

        left.insert("a".to_string(), 1);
        assert_eq!(joined.get_value(&"a".to_string()), Some((1, vec![])));
    }

    #[test]
    fn left_join_pairs_matched_rows() {
        let left = CellMap::<String, i32>::new();
        let right = CellMap::<String, i32>::new();
        let joined = left.clone().left_join(right.clone()).materialize();

        left.insert("a".to_string(), 1);
        right.insert("a".to_string(), 10);
        assert_eq!(joined.get_value(&"a".to_string()), Some((1, vec![10])));
    }

    #[test]
    fn left_join_reacts_to_right_addition() {
        let left = CellMap::<String, i32>::new();
        let right = CellMap::<String, i32>::new();
        let joined = left.clone().left_join(right.clone()).materialize();

        left.insert("a".to_string(), 1);
        assert_eq!(joined.get_value(&"a".to_string()), Some((1, vec![])));

        right.insert("a".to_string(), 10);
        assert_eq!(joined.get_value(&"a".to_string()), Some((1, vec![10])));
    }

    #[test]
    fn left_join_reacts_to_right_removal() {
        let left = CellMap::<String, i32>::new();
        let right = CellMap::<String, i32>::new();
        let joined = left.clone().left_join(right.clone()).materialize();

        left.insert("a".to_string(), 1);
        right.insert("a".to_string(), 10);
        assert_eq!(joined.get_value(&"a".to_string()), Some((1, vec![10])));

        right.remove(&"a".to_string());
        assert_eq!(joined.get_value(&"a".to_string()), Some((1, vec![])));
    }

    #[test]
    fn left_join_reacts_to_left_removal() {
        let left = CellMap::<String, i32>::new();
        let right = CellMap::<String, i32>::new();
        let joined = left.clone().left_join(right.clone()).materialize();

        left.insert("a".to_string(), 1);
        right.insert("a".to_string(), 10);
        assert_eq!(joined.entries().materialize().get().len(), 1);

        left.remove(&"a".to_string());
        assert_eq!(joined.entries().materialize().get().len(), 0);
    }

    #[test]
    fn left_join_by_collects_multiple_right_matches() {
        let left = CellMap::<String, (String, i32)>::new();
        let right = CellMap::<String, (String, i32)>::new();
        let joined = left
            .clone()
            .left_join_by(right.clone(), |_, lv| lv.0.clone(), |_, rv| rv.0.clone())
            .materialize();

        left.insert("l1".to_string(), ("g1".to_string(), 10));
        right.insert("r1".to_string(), ("g1".to_string(), 5));
        right.insert("r2".to_string(), ("g1".to_string(), 7));

        let val = joined.get_value(&"l1".to_string());
        assert!(matches!(
            val,
            Some((left_val, right_vals))
                if left_val == ("g1".to_string(), 10) && right_vals.len() == 2
        ));
    }

    #[test]
    fn left_join_by_keeps_unmatched_with_empty_vec() {
        let left = CellMap::<String, (String, i32)>::new();
        let right = CellMap::<String, (String, i32)>::new();
        let joined = left
            .clone()
            .left_join_by(right, |_, lv| lv.0.clone(), |_, rv| rv.0.clone())
            .materialize();

        left.insert("l1".to_string(), ("g1".to_string(), 10));

        let val = joined.get_value(&"l1".to_string());
        assert!(matches!(
            val,
            Some((left_val, right_vals))
                if left_val == ("g1".to_string(), 10) && right_vals.is_empty()
        ));
    }

    #[test]
    fn left_join_by_preserves_right_batch() {
        let left = CellMap::<String, (String, i32)>::new();
        left.insert("l1".to_string(), ("g1".to_string(), 10));

        let right = CellMap::<String, (String, i32)>::new();
        let joined = left
            .left_join_by(right.clone(), |_, lv| lv.0.clone(), |_, rv| rv.0.clone())
            .materialize();

        let (tx, rx) = mpsc::channel::<MapDiff<String, ((String, i32), Vec<(String, i32)>)>>();
        let _guard = joined.subscribe_diffs(move |diff| {
            let _ = tx.send(diff.clone());
        });

        right.insert_many(vec![
            ("r1".to_string(), ("g1".to_string(), 5)),
            ("r2".to_string(), ("g1".to_string(), 7)),
        ]);

        let seen: Vec<_> = rx.try_iter().collect();
        assert_eq!(seen.len(), 2);
        assert!(matches!(
            seen.last(),
            Some(MapDiff::Batch { changes }) if !changes.is_empty()
        ));
    }

    #[test]
    fn coordinated_two_join_region_tracks_every_root() {
        let left = CellMap::<u64, (u64, i32)>::new();
        let right1 = CellMap::<u64, (u64, i32)>::new();
        let right2 = CellMap::<u64, (u64, i32)>::new();
        let output = left
            .clone()
            .left_join_by(right1.clone(), |_, row| row.0, |_, row| row.0)
            .map_joined_values(|_, left, matches| {
                (
                    left.0,
                    left.1 + matches.iter().map(|(_, row)| row.1).sum::<i32>(),
                )
            })
            .left_join_by(right2.clone(), |_, row| row.0, |_, row| row.0)
            .map_joined_values(|_, middle, matches| {
                middle.1 + matches.iter().map(|(_, row)| row.1).sum::<i32>()
            })
            .materialize();

        left.insert(1, (7, 10));
        assert_eq!(output.get_value(&1), Some(10));

        right1.insert(11, (7, 3));
        assert_eq!(output.get_value(&1), Some(13));

        right2.insert(21, (7, 5));
        assert_eq!(output.get_value(&1), Some(18));

        right1.remove(&11);
        assert_eq!(output.get_value(&1), Some(15));

        right2.remove(&21);
        assert_eq!(output.get_value(&1), Some(10));

        left.remove(&1);
        assert_eq!(output.get_value(&1), None);
    }

    #[test]
    fn coordinated_two_join_region_preserves_batched_updates() {
        let left = CellMap::<u64, (u64, i32)>::new();
        let right1 = CellMap::<u64, (u64, i32)>::new();
        let right2 = CellMap::<u64, (u64, i32)>::new();
        let output = left
            .clone()
            .left_join_by(right1.clone(), |_, row| row.0, |_, row| row.0)
            .map_joined_values(|_, left, matches| {
                (
                    left.0,
                    left.1 + matches.iter().map(|(_, row)| row.1).sum::<i32>(),
                )
            })
            .left_join_by(right2.clone(), |_, row| row.0, |_, row| row.0)
            .map_joined_values(|_, middle, matches| {
                middle.1 + matches.iter().map(|(_, row)| row.1).sum::<i32>()
            })
            .materialize();

        left.insert_many(vec![(1, (7, 10)), (2, (8, 20))]);
        right1.insert_many(vec![(11, (7, 3)), (12, (8, 4))]);
        right2.insert_many(vec![(21, (7, 5)), (22, (8, 6))]);

        assert_eq!(output.get_value(&1), Some(18));
        assert_eq!(output.get_value(&2), Some(30));
    }

    #[test]
    fn coordinated_two_join_region_repartitions_updates_between_joins() {
        let left = CellMap::<u64, (u64, i32)>::new();
        let right1 = CellMap::<u64, (u64, u64, i32)>::new();
        let right2 = CellMap::<u64, (u64, i32)>::new();
        let output = left
            .clone()
            .left_join_by(right1.clone(), |_, row| row.0, |_, row| row.0)
            .map_joined_values(|_, left, matches| {
                let next_relation = matches.first().map_or(0, |(_, row)| row.1);
                let subtotal = left.1 + matches.iter().map(|(_, row)| row.2).sum::<i32>();
                (next_relation, subtotal)
            })
            .left_join_by(right2.clone(), |_, row| row.0, |_, row| row.0)
            .map_joined_values(|_, middle, matches| {
                middle.1 + matches.iter().map(|(_, row)| row.1).sum::<i32>()
            })
            .materialize();

        right2.insert_many(vec![(21, (20, 5)), (31, (30, 7))]);
        right1.insert(11, (10, 20, 3));
        left.insert(1, (10, 100));
        assert_eq!(output.get_value(&1), Some(108));

        // Updating the first relation changes the intermediate join key. The
        // row must leave its old second-stage shard and enter the new one.
        right1.insert(11, (10, 30, 4));
        assert_eq!(output.get_value(&1), Some(111));

        // Moving both sides of the first join exercises route removal and
        // reinsertion while preserving the final map key.
        right1.insert(11, (40, 20, 9));
        assert_eq!(output.get_value(&1), Some(100));
        left.insert(1, (40, 100));
        assert_eq!(output.get_value(&1), Some(114));
    }

    #[test]
    fn coordinated_two_join_matches_reference_across_mixed_root_updates() {
        use std::collections::HashMap;

        let left = CellMap::<u64, (u64, i64)>::new();
        let right1 = CellMap::<u64, (u64, i64)>::new();
        let right2 = CellMap::<u64, (u64, i64)>::new();
        let output = left
            .clone()
            .left_join_by(right1.clone(), |_, row| row.0, |_, row| row.0)
            .map_joined_values(|_, left, matches| {
                let subtotal = left.1 + matches.iter().map(|(_, row)| row.1).sum::<i64>();
                (u64::try_from(subtotal.rem_euclid(8)).unwrap_or(0), subtotal)
            })
            .left_join_by(right2.clone(), |_, row| row.0, |_, row| row.0)
            .map_joined_values(|_, middle, matches| {
                middle.1 + matches.iter().map(|(_, row)| row.1).sum::<i64>()
            })
            .materialize();

        let mut left_reference = HashMap::new();
        let mut right1_reference = HashMap::new();
        let mut right2_reference = HashMap::new();
        let mut random = 0x9e37_79b9_7f4a_7c15_u64;

        for _ in 0..512 {
            random = random
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let key = (random >> 16) % 16;
            let relation = (random >> 32) % 8;
            let value = i64::try_from((random >> 48) % 31).unwrap_or(0) - 15;
            match random % 6 {
                0 => {
                    left.insert(key, (relation, value));
                    left_reference.insert(key, (relation, value));
                }
                1 => {
                    right1.insert(key, (relation, value));
                    right1_reference.insert(key, (relation, value));
                }
                2 => {
                    right2.insert(key, (relation, value));
                    right2_reference.insert(key, (relation, value));
                }
                3 => {
                    left.remove(&key);
                    left_reference.remove(&key);
                }
                4 => {
                    right1.remove(&key);
                    right1_reference.remove(&key);
                }
                _ => {
                    right2.remove(&key);
                    right2_reference.remove(&key);
                }
            }

            for candidate in 0..16 {
                let expected = left_reference.get(&candidate).map(|left_row| {
                    let subtotal = left_row.1
                        + right1_reference
                            .values()
                            .filter(|row| row.0 == left_row.0)
                            .map(|row| row.1)
                            .sum::<i64>();
                    let second_relation = u64::try_from(subtotal.rem_euclid(8)).unwrap_or(0);
                    subtotal
                        + right2_reference
                            .values()
                            .filter(|row| row.0 == second_relation)
                            .map(|row| row.1)
                            .sum::<i64>()
                });
                assert_eq!(output.get_value(&candidate), expected);
            }
        }
    }

    #[test]
    fn joined_projection_preserves_right_insertion_order() {
        let left = CellMap::<u64, u64>::new();
        let right = CellMap::<u64, u64>::new();
        let output = left
            .clone()
            .left_join_by(right.clone(), |_, group| *group, |_, group| *group)
            .map_joined_values(|_, _, matches| {
                matches.iter().map(|(key, _)| *key).collect::<Vec<_>>()
            })
            .materialize();

        left.insert(1, 7);
        right.insert_many(vec![(30, 7), (10, 7), (20, 7)]);
        assert_eq!(output.get_value(&1), Some(vec![30, 10, 20]));

        right.insert(10, 7);
        assert_eq!(output.get_value(&1), Some(vec![30, 10, 20]));

        right.remove(&10);
        right.insert(10, 7);
        assert_eq!(output.get_value(&1), Some(vec![30, 20, 10]));
    }

    #[test]
    fn right_changes_publish_impacted_left_rows_in_insertion_order() {
        let left = CellMap::<u64, u64>::new();
        let right = CellMap::<u64, u64>::new();
        let output = left
            .clone()
            .left_join_by(right.clone(), |_, group| *group, |_, group| *group)
            .map_joined_values(|_, _, matches| matches.len())
            .materialize();

        left.insert_many(vec![(9, 7), (3, 7), (7, 7)]);
        let (tx, rx) = mpsc::channel::<MapDiff<u64, usize>>();
        let _guard = output.subscribe_diffs(move |diff| {
            let _ = tx.send(diff.clone());
        });

        right.insert(1, 7);

        let diff = rx.try_iter().last();
        assert!(diff.is_some(), "right insert must publish");
        let keys: Vec<u64> = match diff {
            Some(MapDiff::Batch { changes }) => changes
                .iter()
                .filter_map(|change| match change {
                    MapDiff::Update { key, .. } => Some(*key),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        };
        assert_eq!(keys, vec![9, 3, 7]);
    }

    #[cfg(feature = "scheduler")]
    #[test]
    fn large_parallel_join_batch_settles_synchronously_in_input_order() {
        const ROWS: u64 = 10_000;

        let left = CellMap::<u64, (u64, u64)>::new();
        let right1 = CellMap::<u64, (u64, u64)>::new();
        let right2 = CellMap::<u64, (u64, u64)>::new();
        right1.insert_many((0..8).map(|key| (key, (1, key))).collect());
        right2.insert_many((0..8).map(|key| (key, (1, key))).collect());

        let output = left
            .clone()
            .left_join_by(right1, |_, row| row.0, |_, row| row.0)
            .map_joined_values(|_, row, matches| {
                (
                    row.0,
                    row.1 + u64::try_from(matches.len()).unwrap_or(u64::MAX),
                )
            })
            .left_join_by(right2, |_, row| row.0, |_, row| row.0)
            .map_joined_values(|_, row, matches| {
                row.1 + u64::try_from(matches.len()).unwrap_or(u64::MAX)
            })
            .materialize();

        let (tx, rx) = mpsc::channel::<MapDiff<u64, u64>>();
        let _guard = output.subscribe_diffs(move |diff| {
            let _ = tx.send(diff.clone());
        });

        left.insert_many((0..ROWS).map(|key| (key, (1, key))).collect());

        assert_eq!(output.get_value(&(ROWS - 1)), Some(ROWS - 1 + 16));
        let diff = rx.try_iter().last();
        assert!(diff.is_some());
        let keys: Vec<u64> = match diff {
            Some(MapDiff::Batch { changes }) => changes
                .iter()
                .filter_map(|change| match change {
                    MapDiff::Insert { key, .. } => Some(*key),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        };
        assert_eq!(keys, (0..ROWS).collect::<Vec<_>>());
    }

    #[derive(Debug, Clone, PartialEq, Eq, Hash)]
    struct UserId(String);

    #[derive(Debug, Clone, PartialEq)]
    struct User {
        name: String,
    }

    impl IdFor<User> for UserId {
        type MapKey = String;
        fn map_key(&self) -> String {
            self.0.clone()
        }
    }

    impl IdType for UserId {
        type Parent = User;
    }

    #[derive(Debug, Clone, PartialEq)]
    struct Post {
        user_id: UserId,
        title: String,
    }

    struct UserPosts;

    impl ForeignKeyRelation for UserPosts {
        type Parent = User;
        type Child = Post;
        type ForeignKey = UserId;

        fn foreign_key(post: &Post) -> Option<UserId> {
            (!post.user_id.0.is_empty()).then(|| post.user_id.clone())
        }
    }

    #[test]
    fn left_join_fk_keeps_unmatched_with_empty_vec() {
        let users = CellMap::<String, User>::new();
        let posts = CellMap::<String, Post>::new();
        let joined = users
            .clone()
            .left_join_fk::<UserPosts, _>(posts)
            .materialize();

        users.insert(
            "u1".to_string(),
            User {
                name: "Alice".to_string(),
            },
        );

        let val = joined.get_value(&"u1".to_string());
        assert!(matches!(
            val,
            Some((user, posts)) if user.name == "Alice" && posts.is_empty()
        ));
    }

    #[test]
    fn left_join_fk_ignores_absent_foreign_keys() {
        let users = CellMap::<String, User>::new();
        let posts = CellMap::<String, Post>::new();
        let joined = users
            .clone()
            .left_join_fk::<UserPosts, _>(posts.clone())
            .materialize();
        users.insert(
            "u1".to_string(),
            User {
                name: "Alice".to_string(),
            },
        );
        posts.insert(
            "p1".to_string(),
            Post {
                user_id: UserId(String::new()),
                title: "Orphan".to_string(),
            },
        );
        assert!(matches!(joined.get_value(&"u1".to_string()), Some((_, rows)) if rows.is_empty()));
        posts.insert(
            "p1".to_string(),
            Post {
                user_id: UserId("u1".to_string()),
                title: "Attached".to_string(),
            },
        );
        assert!(matches!(joined.get_value(&"u1".to_string()), Some((_, rows)) if rows.len() == 1));
        posts.insert(
            "p1".to_string(),
            Post {
                user_id: UserId(String::new()),
                title: "Detached".to_string(),
            },
        );
        assert!(matches!(joined.get_value(&"u1".to_string()), Some((_, rows)) if rows.is_empty()));
    }

    #[test]
    fn left_join_fk_collects_matching_posts() {
        let users = CellMap::<String, User>::new();
        let posts = CellMap::<String, Post>::new();
        let joined = users
            .clone()
            .left_join_fk::<UserPosts, _>(posts.clone())
            .materialize();

        users.insert(
            "u1".to_string(),
            User {
                name: "Alice".to_string(),
            },
        );
        posts.insert(
            "p1".to_string(),
            Post {
                user_id: UserId("u1".to_string()),
                title: "Hello".to_string(),
            },
        );
        posts.insert(
            "p2".to_string(),
            Post {
                user_id: UserId("u1".to_string()),
                title: "World".to_string(),
            },
        );

        let val = joined.get_value(&"u1".to_string());
        assert!(matches!(
            val,
            Some((_, matched_posts)) if matched_posts.len() == 2
        ));
    }

    #[test]
    fn fk_join_survives_key_preserving_parent_projection() {
        let users = CellMap::<String, User>::new();
        let posts = CellMap::<String, Post>::new();
        let joined = users
            .clone()
            .map_values(|_key, user| user.name.to_uppercase())
            .left_join_fk::<UserPosts, _>(posts.clone())
            .materialize();

        users.insert(
            "u1".to_string(),
            User {
                name: "Alice".to_string(),
            },
        );
        posts.insert(
            "p1".to_string(),
            Post {
                user_id: UserId("u1".to_string()),
                title: "Hello".to_string(),
            },
        );

        assert!(matches!(
            joined.get_value(&"u1".to_string()),
            Some((name, matches)) if name == "ALICE" && matches.len() == 1
        ));
    }

    #[test]
    fn repeated_fk_relationship_keeps_distinct_projected_right_inputs() {
        let users = CellMap::<String, User>::new();
        let posts = CellMap::<String, Post>::new();
        let projected_a = posts.clone().map_values(|_, post| Post {
            user_id: post.user_id.clone(),
            title: format!("{}-a", post.title),
        });
        let projected_b = posts.clone().map_values(|_, post| Post {
            user_id: post.user_id.clone(),
            title: format!("{}-b", post.title),
        });
        let joined = users
            .clone()
            .left_join_fk::<UserPosts, _>(projected_a)
            .map_joined_values(|_, user, first_posts| {
                (
                    user.clone(),
                    first_posts.first().map(|(_, post)| post.title.clone()),
                )
            })
            .left_join_fk::<UserPosts, _>(projected_b)
            .map_joined_values(|_, first, second_posts| {
                (
                    first.1.clone(),
                    second_posts.first().map(|(_, post)| post.title.clone()),
                )
            })
            .materialize();

        users.insert(
            "u1".to_string(),
            User {
                name: "Alice".to_string(),
            },
        );
        posts.insert(
            "p1".to_string(),
            Post {
                user_id: UserId("u1".to_string()),
                title: "First".to_string(),
            },
        );

        assert_eq!(
            joined.get_value(&"u1".to_string()),
            Some((Some("First-a".to_string()), Some("First-b".to_string())))
        );

        posts.insert(
            "p1".to_string(),
            Post {
                user_id: UserId("u1".to_string()),
                title: "Updated".to_string(),
            },
        );
        assert_eq!(
            joined.get_value(&"u1".to_string()),
            Some((Some("Updated-a".to_string()), Some("Updated-b".to_string())))
        );
    }

    #[test]
    fn repeated_fk_relationship_reuses_index_and_updates_every_join() {
        let users = CellMap::<String, User>::new();
        let posts = CellMap::<String, Post>::new();
        let joined = users
            .clone()
            .left_join_fk::<UserPosts, _>(posts.clone())
            .map_joined_values(|_, user, first_posts| (user.clone(), first_posts.len()))
            .left_join_fk::<UserPosts, _>(posts.clone())
            .map_joined_values(|_, first, second_posts| (first.1, second_posts.len()))
            .materialize();

        users.insert(
            "u1".to_string(),
            User {
                name: "Alice".to_string(),
            },
        );
        users.insert(
            "u2".to_string(),
            User {
                name: "Bob".to_string(),
            },
        );
        posts.insert(
            "p1".to_string(),
            Post {
                user_id: UserId("u1".to_string()),
                title: "First".to_string(),
            },
        );

        assert_eq!(joined.get_value(&"u1".to_string()), Some((1, 1)));
        assert_eq!(joined.get_value(&"u2".to_string()), Some((0, 0)));

        posts.insert(
            "p1".to_string(),
            Post {
                user_id: UserId("u2".to_string()),
                title: "Moved".to_string(),
            },
        );

        assert_eq!(joined.get_value(&"u1".to_string()), Some((0, 0)));
        assert_eq!(joined.get_value(&"u2".to_string()), Some((1, 1)));

        posts.insert(
            "p1".to_string(),
            Post {
                user_id: UserId(String::new()),
                title: "Detached".to_string(),
            },
        );
        assert_eq!(joined.get_value(&"u1".to_string()), Some((0, 0)));
        assert_eq!(joined.get_value(&"u2".to_string()), Some((0, 0)));
    }

    #[derive(Debug, Clone, PartialEq)]
    struct OptionalPost {
        user_id: Option<UserId>,
        sequence: usize,
    }

    struct OptionalUserPosts;

    impl ForeignKeyRelation for OptionalUserPosts {
        type Parent = User;
        type Child = OptionalPost;
        type ForeignKey = UserId;

        fn foreign_key(post: &OptionalPost) -> Option<UserId> {
            post.user_id.clone()
        }
    }

    #[test]
    fn repeated_optional_fk_relationship_tracks_some_none_transitions() {
        let users = CellMap::<String, User>::new();
        let posts = CellMap::<String, OptionalPost>::new();
        let joined = users
            .clone()
            .left_join_fk::<OptionalUserPosts, _>(posts.clone())
            .map_joined_values(|_, _, first| first.len())
            .left_join_fk::<OptionalUserPosts, _>(posts.clone())
            .map_joined_values(|_, first, second| (*first, second.len()))
            .materialize();
        users.insert(
            "u1".into(),
            User {
                name: "Alice".into(),
            },
        );
        posts.insert(
            "p1".into(),
            OptionalPost {
                user_id: None,
                sequence: 1,
            },
        );
        assert_eq!(joined.get_value(&"u1".to_string()), Some((0, 0)));

        posts.insert(
            "p1".into(),
            OptionalPost {
                user_id: Some(UserId("u1".into())),
                sequence: 2,
            },
        );
        assert_eq!(joined.get_value(&"u1".to_string()), Some((1, 1)));

        posts.insert(
            "p1".into(),
            OptionalPost {
                user_id: None,
                sequence: 3,
            },
        );
        assert_eq!(joined.get_value(&"u1".to_string()), Some((0, 0)));
    }

    #[cfg(feature = "scheduler")]
    #[test]
    fn large_sharded_optional_fk_batch_omits_absent_routes() {
        const ROWS: usize = 66_000;
        let users = CellMap::<String, User>::new();
        let posts = CellMap::<usize, OptionalPost>::new();
        let joined = users
            .clone()
            .left_join_fk::<OptionalUserPosts, _>(posts.clone())
            .map_joined_values(|_, _, first| first.len())
            .left_join_fk::<OptionalUserPosts, _>(posts.clone())
            .map_joined_values(|_, first, second| (*first, second.len()))
            .materialize();
        users.insert(
            "u1".into(),
            User {
                name: "Alice".into(),
            },
        );
        posts.insert_many(
            (0..ROWS)
                .map(|sequence| {
                    (
                        sequence,
                        OptionalPost {
                            user_id: Some(UserId("u1".into())),
                            sequence,
                        },
                    )
                })
                .collect(),
        );
        assert_eq!(joined.get_value(&"u1".to_string()), Some((ROWS, ROWS)));

        posts.insert_many(
            (0..ROWS)
                .map(|sequence| {
                    (
                        sequence,
                        OptionalPost {
                            user_id: None,
                            sequence,
                        },
                    )
                })
                .collect(),
        );
        assert_eq!(joined.get_value(&"u1".to_string()), Some((0, 0)));
    }
}
