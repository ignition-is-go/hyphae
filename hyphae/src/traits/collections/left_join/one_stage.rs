use std::{hash::Hash, marker::PhantomData};

use crate::{
    map_query::{
        BuildQueryRuntime, MapQuery,
        properties::{ByMapKey, ByRelation, ExactlyOne, PlanProperties},
    },
    subscription::SubscriptionGuard,
    traits::{
        CellValue, RightJoinKey,
        collections::internal::{
            join_region::StageProject, join_runtime::install_keyed_join_runtime_via_query,
        },
    },
};

/// A query node carrying the semantic identity of a typed relationship.
#[doc(hidden)]
pub struct RelationPlan<P, Rel> {
    pub(super) plan: P,
    pub(super) _relation: PhantomData<fn() -> Rel>,
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

/// Plan node for [`crate::traits::collections::LeftJoinExt::left_join`],
/// [`crate::traits::collections::LeftJoinExt::left_join_fk`], and
/// [`crate::traits::collections::LeftJoinExt::left_join_by`].
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
pub struct TupleJoinProjection<F>(pub(super) F);

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
pub struct DirectJoinProjection<F>(pub(super) F);

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
    pub(super) join: LeftJoinPlan<L, R, LK, LV, RK, RV, JK, FL, FR>,
    pub(super) projection: F,
    pub(super) _output: PhantomData<fn() -> OV>,
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
