//! Left-join plan nodes implementing [`MapQuery`].
//!
//! `left_join`, `left_join_fk`, and `left_join_by` build uncompiled plan nodes
//! that compose with other [`MapQuery`] operators. Call
//! [`MapQuery::materialize`] to compile a plan into a subscribable
//! [`CellMap`](crate::CellMap).

use std::{hash::Hash, marker::PhantomData};

use crate::{
    map_query::MapQuery,
    traits::{CellValue, ForeignKeyRelation, IdFor, OptionalRightKey, RequiredRightKey},
};

mod one_stage;
mod two_stage;

pub use one_stage::{
    DirectJoinProjection, JoinProjection, JoinProjectionProject, JoinedValuesPlan, LeftJoinPlan,
    RelationPlan, TupleJoinProjection,
};
pub use two_stage::{TwoLeftJoinMappedPlan, TwoLeftJoinPlan};

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
