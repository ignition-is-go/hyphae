//! Inner-join plan nodes implementing [`MapQuery`].
//!
//! `inner_join`, `inner_join_fk`, and `inner_join_by` build uncompiled plan
//! nodes that compose with other [`MapQuery`] operators. Call
//! [`MapQuery::materialize`] to compile a plan into a subscribable
//! [`CellMap`].

use std::{hash::Hash, marker::PhantomData};

use crate::{
    map_query::{MapDiffSink, MapQuery, MapQueryInstall},
    subscription::SubscriptionGuard,
    traits::{
        CellValue, HasForeignKey, IdFor,
        collections::internal::join_runtime::install_join_runtime_via_query,
    },
};

/// Plan node for [`InnerJoinExt::inner_join`] (key-equal inner join).
///
/// Output rows are keyed by the shared map key with value `(LV, RV)`.
/// Not [`Clone`]: cloning a plan would silently duplicate join work; share by
/// materializing once.
pub struct InnerJoinByKeyPlan<L, R, K, LV, RV>
where
    L: MapQuery<K, LV>,
    R: MapQuery<K, RV>,
    K: Hash + Eq + CellValue,
    LV: CellValue,
    RV: CellValue,
{
    pub(crate) left: L,
    pub(crate) right: R,
    #[allow(clippy::type_complexity)]
    pub(crate) _types: PhantomData<fn() -> (K, LV, RV)>,
}

impl<L, R, K, LV, RV> MapQueryInstall<K, (LV, RV)> for InnerJoinByKeyPlan<L, R, K, LV, RV>
where
    L: MapQuery<K, LV>,
    R: MapQuery<K, RV>,
    K: Hash + Eq + CellValue,
    LV: CellValue,
    RV: CellValue,
{
    fn install(self, sink: MapDiffSink<K, (LV, RV)>) -> Vec<SubscriptionGuard> {
        install_join_runtime_via_query::<K, LV, K, RV, K, K, (LV, RV), _, _, _, _, _>(
            self.left,
            self.right,
            |k: &K, _: &LV| k.clone(),
            |k: &K, _: &RV| k.clone(),
            |left_k: &K, left_v: &LV, rights: &[(K, RV)]| {
                rights
                    .iter()
                    .map(|(_, rv)| (left_k.clone(), (left_v.clone(), rv.clone())))
                    .collect()
            },
            sink,
        )
    }
}

#[allow(private_bounds)]
impl<L, R, K, LV, RV> MapQuery<K, (LV, RV)> for InnerJoinByKeyPlan<L, R, K, LV, RV>
where
    L: MapQuery<K, LV>,
    R: MapQuery<K, RV>,
    K: Hash + Eq + CellValue,
    LV: CellValue,
    RV: CellValue,
{
}

/// Plan node for [`InnerJoinExt::inner_join_by`] and
/// [`InnerJoinExt::inner_join_fk`] (key-extractor inner joins).
///
/// Output rows are keyed by `(LK, RK)` with value `(LV, RV)`.
/// Not [`Clone`]: cloning a plan would silently duplicate join work; share by
/// materializing once.
pub struct InnerJoinByPairPlan<L, R, LK, LV, RK, RV, JK, FL, FR>
where
    L: MapQuery<LK, LV>,
    R: MapQuery<RK, RV>,
    LK: Hash + Eq + CellValue,
    LV: CellValue,
    RK: Hash + Eq + CellValue,
    RV: CellValue,
    JK: Hash + Eq + CellValue,
    FL: Fn(&LK, &LV) -> JK + Send + Sync + 'static,
    FR: Fn(&RK, &RV) -> JK + Send + Sync + 'static,
{
    pub(crate) left: L,
    pub(crate) right: R,
    pub(crate) left_key: FL,
    pub(crate) right_key: FR,
    #[allow(clippy::type_complexity)]
    pub(crate) _types: PhantomData<fn() -> (LK, LV, RK, RV, JK)>,
}

impl<L, R, LK, LV, RK, RV, JK, FL, FR> MapQueryInstall<(LK, RK), (LV, RV)>
    for InnerJoinByPairPlan<L, R, LK, LV, RK, RV, JK, FL, FR>
where
    L: MapQuery<LK, LV>,
    R: MapQuery<RK, RV>,
    LK: Hash + Eq + CellValue,
    LV: CellValue,
    RK: Hash + Eq + CellValue,
    RV: CellValue,
    JK: Hash + Eq + CellValue,
    FL: Fn(&LK, &LV) -> JK + Send + Sync + 'static,
    FR: Fn(&RK, &RV) -> JK + Send + Sync + 'static,
{
    fn install(self, sink: MapDiffSink<(LK, RK), (LV, RV)>) -> Vec<SubscriptionGuard> {
        install_join_runtime_via_query::<LK, LV, RK, RV, JK, (LK, RK), (LV, RV), _, _, _, _, _>(
            self.left,
            self.right,
            self.left_key,
            self.right_key,
            |left_k: &LK, left_v: &LV, rights: &[(RK, RV)]| {
                rights
                    .iter()
                    .map(|(rk, rv)| ((left_k.clone(), rk.clone()), (left_v.clone(), rv.clone())))
                    .collect()
            },
            sink,
        )
    }
}

#[allow(private_bounds)]
impl<L, R, LK, LV, RK, RV, JK, FL, FR> MapQuery<(LK, RK), (LV, RV)>
    for InnerJoinByPairPlan<L, R, LK, LV, RK, RV, JK, FL, FR>
where
    L: MapQuery<LK, LV>,
    R: MapQuery<RK, RV>,
    LK: Hash + Eq + CellValue,
    LV: CellValue,
    RK: Hash + Eq + CellValue,
    RV: CellValue,
    JK: Hash + Eq + CellValue,
    FL: Fn(&LK, &LV) -> JK + Send + Sync + 'static,
    FR: Fn(&RK, &RV) -> JK + Send + Sync + 'static,
{
}

/// Inner-join operators returning [`MapQuery`] plan nodes.
///
/// All three methods consume `self` and return uncompiled plan nodes; call
/// [`MapQuery::materialize`] on the result to obtain a subscribable
/// [`CellMap`](crate::CellMap).
pub trait InnerJoinExt<K, V>: MapQuery<K, V>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    /// Inner join on equal map keys.
    ///
    /// Pairs left and right rows that share the same map key.
    /// Produces one output row per match, keyed by the shared key.
    /// Unmatched rows from either side are excluded.
    fn inner_join<R, RV>(self, right: R) -> InnerJoinByKeyPlan<Self, R, K, V, RV>
    where
        R: MapQuery<K, RV>,
        RV: CellValue,
    {
        InnerJoinByKeyPlan {
            left: self,
            right,
            _types: PhantomData,
        }
    }

    /// Inner join using foreign key relationship.
    ///
    /// Joins on the left map key matching the right value's foreign key.
    /// Produces one output row per matching (left, right) pair, keyed by
    /// `(K, RK)`. Unmatched rows from either side are excluded.
    #[allow(clippy::type_complexity)]
    fn inner_join_fk<R, RK, RV>(
        self,
        right: R,
    ) -> InnerJoinByPairPlan<
        Self,
        R,
        K,
        V,
        RK,
        RV,
        K,
        impl Fn(&K, &V) -> K + Send + Sync + 'static,
        impl Fn(&RK, &RV) -> K + Send + Sync + 'static,
    >
    where
        R: MapQuery<RK, RV>,
        RK: Hash + Eq + CellValue,
        RV: CellValue + HasForeignKey<V>,
        <<RV as HasForeignKey<V>>::ForeignKey as IdFor<V>>::MapKey: Into<K>,
    {
        InnerJoinByPairPlan {
            left: self,
            right,
            left_key: |k: &K, _: &V| k.clone(),
            right_key: |_: &RK, rv: &RV| rv.fk().map_key().into(),
            _types: PhantomData,
        }
    }

    /// Inner join using explicit key extractors.
    ///
    /// `left_key` and `right_key` extract the join key from each side.
    /// Produces one output row per matching (left, right) pair, keyed by
    /// `(K, RK)`. Unmatched rows from either side are excluded.
    fn inner_join_by<R, RK, RV, JK, FL, FR>(
        self,
        right: R,
        left_key: FL,
        right_key: FR,
    ) -> InnerJoinByPairPlan<Self, R, K, V, RK, RV, JK, FL, FR>
    where
        R: MapQuery<RK, RV>,
        RK: Hash + Eq + CellValue,
        RV: CellValue,
        JK: Hash + Eq + CellValue,
        FL: Fn(&K, &V) -> JK + Send + Sync + 'static,
        FR: Fn(&RK, &RV) -> JK + Send + Sync + 'static,
    {
        InnerJoinByPairPlan {
            left: self,
            right,
            left_key,
            right_key,
            _types: PhantomData,
        }
    }
}

impl<K, V, M> InnerJoinExt<K, V> for M
where
    K: Hash + Eq + CellValue,
    V: CellValue,
    M: MapQuery<K, V>,
{
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;
    use crate::{
        CellMap, MapDiff,
        traits::{Gettable, HasForeignKey, IdFor, IdType},
    };

    #[test]
    fn inner_join_pairs_on_equal_keys() {
        let left = CellMap::<String, i32>::new();
        let right = CellMap::<String, i32>::new();
        let joined = left.clone().inner_join(right.clone()).materialize();

        left.insert("a".to_string(), 1);
        assert_eq!(joined.entries().get().len(), 0);

        right.insert("a".to_string(), 10);
        assert_eq!(joined.get_value(&"a".to_string()), Some((1, 10)));
    }

    #[test]
    fn inner_join_excludes_unmatched() {
        let left = CellMap::<String, i32>::new();
        let right = CellMap::<String, i32>::new();
        let joined = left.clone().inner_join(right.clone()).materialize();

        left.insert("a".to_string(), 1);
        left.insert("b".to_string(), 2);
        right.insert("a".to_string(), 10);

        assert_eq!(joined.entries().get().len(), 1);
        assert_eq!(joined.get_value(&"a".to_string()), Some((1, 10)));
        assert_eq!(joined.get_value(&"b".to_string()), None);
    }

    #[test]
    fn inner_join_reacts_to_updates() {
        let left = CellMap::<String, i32>::new();
        let right = CellMap::<String, i32>::new();
        let joined = left.clone().inner_join(right.clone()).materialize();

        left.insert("a".to_string(), 1);
        right.insert("a".to_string(), 10);
        assert_eq!(joined.get_value(&"a".to_string()), Some((1, 10)));

        right.insert("a".to_string(), 20);
        assert_eq!(joined.get_value(&"a".to_string()), Some((1, 20)));
    }

    #[test]
    fn inner_join_reacts_to_removals() {
        let left = CellMap::<String, i32>::new();
        let right = CellMap::<String, i32>::new();
        let joined = left.clone().inner_join(right.clone()).materialize();

        left.insert("a".to_string(), 1);
        right.insert("a".to_string(), 10);
        assert_eq!(joined.entries().get().len(), 1);

        right.remove(&"a".to_string());
        assert_eq!(joined.entries().get().len(), 0);
    }

    #[test]
    fn inner_join_by_produces_composite_keys() {
        let left = CellMap::<String, (String, i32)>::new();
        let right = CellMap::<String, (String, i32)>::new();
        let joined = left
            .clone()
            .inner_join_by(right.clone(), |_, lv| lv.0.clone(), |_, rv| rv.0.clone())
            .materialize();

        left.insert("l1".to_string(), ("g1".to_string(), 10));
        right.insert("r1".to_string(), ("g1".to_string(), 5));

        let key = ("l1".to_string(), "r1".to_string());
        let val = joined.get_value(&key);
        assert_eq!(val, Some((("g1".to_string(), 10), ("g1".to_string(), 5))));
    }

    #[test]
    fn inner_join_by_handles_one_to_many() {
        let left = CellMap::<String, (String, i32)>::new();
        let right = CellMap::<String, (String, i32)>::new();
        let joined = left
            .clone()
            .inner_join_by(right.clone(), |_, lv| lv.0.clone(), |_, rv| rv.0.clone())
            .materialize();

        left.insert("l1".to_string(), ("g1".to_string(), 10));
        right.insert("r1".to_string(), ("g1".to_string(), 5));
        right.insert("r2".to_string(), ("g1".to_string(), 7));

        assert_eq!(joined.entries().get().len(), 2);
    }

    #[test]
    fn inner_join_by_preserves_right_batch() {
        let left = CellMap::<String, (String, i32)>::new();
        left.insert("l1".to_string(), ("g1".to_string(), 10));

        let right = CellMap::<String, (String, i32)>::new();
        let joined = left
            .clone()
            .inner_join_by(right.clone(), |_, lv| lv.0.clone(), |_, rv| rv.0.clone())
            .materialize();

        let (tx, rx) = mpsc::channel::<MapDiff<(String, String), ((String, i32), (String, i32))>>();
        let _guard = joined.subscribe_diffs(move |diff| {
            let _ = tx.send(diff.clone());
        });

        right.insert_many(vec![
            ("r1".to_string(), ("g1".to_string(), 5)),
            ("r2".to_string(), ("g1".to_string(), 7)),
        ]);

        let seen: Vec<_> = rx.try_iter().collect();
        assert_eq!(seen.len(), 2);
        match seen.last().expect("last diff") {
            MapDiff::Batch { changes } => assert_eq!(changes.len(), 2),
            _ => panic!("expected batch diff from inner_join_by"),
        }
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

    impl HasForeignKey<User> for Post {
        type ForeignKey = UserId;
        fn fk(&self) -> UserId {
            self.user_id.clone()
        }
    }

    #[test]
    fn inner_join_fk_pairs_on_foreign_key() {
        let users = CellMap::<String, User>::new();
        let posts = CellMap::<String, Post>::new();
        let joined = users.clone().inner_join_fk(posts.clone()).materialize();

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

        let key = ("u1".to_string(), "p1".to_string());
        let val = joined.get_value(&key);
        assert_eq!(
            val,
            Some((
                User {
                    name: "Alice".to_string()
                },
                Post {
                    user_id: UserId("u1".to_string()),
                    title: "Hello".to_string()
                },
            ))
        );
    }

    #[test]
    fn inner_join_fk_handles_one_to_many() {
        let users = CellMap::<String, User>::new();
        let posts = CellMap::<String, Post>::new();
        let joined = users.clone().inner_join_fk(posts.clone()).materialize();

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

        assert_eq!(joined.entries().get().len(), 2);
    }

    #[test]
    fn inner_join_fk_excludes_unmatched() {
        let users = CellMap::<String, User>::new();
        let posts = CellMap::<String, Post>::new();
        let joined = users.clone().inner_join_fk(posts.clone()).materialize();

        users.insert(
            "u1".to_string(),
            User {
                name: "Alice".to_string(),
            },
        );
        posts.insert(
            "p1".to_string(),
            Post {
                user_id: UserId("u2".to_string()),
                title: "Orphan".to_string(),
            },
        );

        assert_eq!(joined.entries().get().len(), 0);
    }
}
