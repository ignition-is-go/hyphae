//! Left semi-join plan nodes implementing [`MapQuery`].
//!
//! `left_semi_join`, `left_semi_join_fk`, and `left_semi_join_by` build
//! uncompiled plan nodes that compose with other [`MapQuery`] operators. Call
//! [`MapQuery::materialize`] to compile a plan into a subscribable
//! [`CellMap`](crate::CellMap).

use std::{hash::Hash, marker::PhantomData};

use crate::{
    map_query::{MapDiffSink, MapQuery, MapQueryInstall},
    subscription::SubscriptionGuard,
    traits::{
        CellValue, HasForeignKey, IdFor,
        collections::internal::join_runtime::install_join_runtime_via_query,
    },
};

/// Plan node for [`LeftSemiJoinExt::left_semi_join`],
/// [`LeftSemiJoinExt::left_semi_join_fk`], and
/// [`LeftSemiJoinExt::left_semi_join_by`].
///
/// Keeps left rows that have at least one matching right row. Unmatched left
/// rows are excluded. Output value is the left value (no right tuple), keyed
/// by the left key.
///
/// Not [`Clone`]: cloning a plan would silently duplicate join work; share by
/// materializing once.
pub struct LeftSemiJoinPlan<L, R, LK, LV, RK, RV, JK, FL, FR>
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

impl<L, R, LK, LV, RK, RV, JK, FL, FR> MapQueryInstall<LK, LV>
    for LeftSemiJoinPlan<L, R, LK, LV, RK, RV, JK, FL, FR>
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
    fn install(self, sink: MapDiffSink<LK, LV>) -> Vec<SubscriptionGuard> {
        install_join_runtime_via_query::<LK, LV, RK, RV, JK, LK, LV, _, _, _, _, _>(
            self.left,
            self.right,
            self.left_key,
            self.right_key,
            |left_k: &LK, left_v: &LV, rights: &[(RK, RV)]| {
                if rights.is_empty() {
                    Vec::new()
                } else {
                    vec![(left_k.clone(), left_v.clone())]
                }
            },
            sink,
        )
    }
}

#[allow(private_bounds)]
impl<L, R, LK, LV, RK, RV, JK, FL, FR> MapQuery<LK, LV>
    for LeftSemiJoinPlan<L, R, LK, LV, RK, RV, JK, FL, FR>
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

/// Left semi-join operators returning [`MapQuery`] plan nodes.
///
/// All three methods consume `self` and return uncompiled plan nodes; call
/// [`MapQuery::materialize`] on the result to obtain a subscribable
/// [`CellMap`](crate::CellMap).
pub trait LeftSemiJoinExt<K, V>: MapQuery<K, V>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    /// Left semi join on equal map keys.
    ///
    /// Keeps left rows where a right row with the same key exists. Unmatched
    /// left rows are excluded. Output contains only left data.
    #[allow(clippy::type_complexity)]
    fn left_semi_join<R, RV>(
        self,
        right: R,
    ) -> LeftSemiJoinPlan<
        Self,
        R,
        K,
        V,
        K,
        RV,
        K,
        impl Fn(&K, &V) -> K + Send + Sync + 'static,
        impl Fn(&K, &RV) -> K + Send + Sync + 'static,
    >
    where
        R: MapQuery<K, RV>,
        RV: CellValue,
    {
        LeftSemiJoinPlan {
            left: self,
            right,
            left_key: |k: &K, _: &V| k.clone(),
            right_key: |k: &K, _: &RV| k.clone(),
            _types: PhantomData,
        }
    }

    /// Left semi join using foreign key relationship.
    ///
    /// Joins on the left map key matching the right value's foreign key.
    /// Keeps left rows that have at least one matching right row. Unmatched
    /// left rows are excluded. Output contains only left data.
    #[allow(clippy::type_complexity)]
    fn left_semi_join_fk<R, RK, RV>(
        self,
        right: R,
    ) -> LeftSemiJoinPlan<
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
        LeftSemiJoinPlan {
            left: self,
            right,
            left_key: |k: &K, _: &V| k.clone(),
            right_key: |_: &RK, rv: &RV| rv.fk().map_key().into(),
            _types: PhantomData,
        }
    }

    /// Left semi join using explicit key extractors.
    ///
    /// `left_key` and `right_key` extract the join key from each side.
    /// Keeps left rows that have at least one matching right row. Unmatched
    /// left rows are excluded. Output contains only left data.
    fn left_semi_join_by<R, RK, RV, JK, FL, FR>(
        self,
        right: R,
        left_key: FL,
        right_key: FR,
    ) -> LeftSemiJoinPlan<Self, R, K, V, RK, RV, JK, FL, FR>
    where
        R: MapQuery<RK, RV>,
        RK: Hash + Eq + CellValue,
        RV: CellValue,
        JK: Hash + Eq + CellValue,
        FL: Fn(&K, &V) -> JK + Send + Sync + 'static,
        FR: Fn(&RK, &RV) -> JK + Send + Sync + 'static,
    {
        LeftSemiJoinPlan {
            left: self,
            right,
            left_key,
            right_key,
            _types: PhantomData,
        }
    }
}

impl<K, V, M> LeftSemiJoinExt<K, V> for M
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
    fn left_semi_join_keeps_matched_rows() {
        let left = CellMap::<String, i32>::new();
        let right = CellMap::<String, bool>::new();
        let joined = left.clone().left_semi_join(right.clone()).materialize();

        left.insert("a".to_string(), 1);
        assert_eq!(joined.entries().get().len(), 0);

        right.insert("a".to_string(), true);
        assert_eq!(joined.get_value(&"a".to_string()), Some(1));
    }

    #[test]
    fn left_semi_join_drops_unmatched_rows() {
        let left = CellMap::<String, i32>::new();
        let right = CellMap::<String, bool>::new();
        let joined = left.clone().left_semi_join(right.clone()).materialize();

        left.insert("a".to_string(), 1);
        left.insert("b".to_string(), 2);
        right.insert("a".to_string(), true);

        assert_eq!(joined.entries().get().len(), 1);
        assert_eq!(joined.get_value(&"a".to_string()), Some(1));
        assert_eq!(joined.get_value(&"b".to_string()), None);
    }

    #[test]
    fn left_semi_join_reacts_to_right_removal() {
        let left = CellMap::<String, i32>::new();
        let right = CellMap::<String, bool>::new();
        let joined = left.clone().left_semi_join(right.clone()).materialize();

        left.insert("a".to_string(), 1);
        right.insert("a".to_string(), true);
        assert_eq!(joined.entries().get().len(), 1);

        right.remove(&"a".to_string());
        assert_eq!(joined.entries().get().len(), 0);
    }

    #[test]
    fn left_semi_join_by_uses_custom_keys() {
        let left = CellMap::<String, i32>::new();
        let right = CellMap::<String, i32>::new();
        let joined = left
            .clone()
            .left_semi_join_by(right.clone(), |_, lv| lv.to_string(), |rk, _| rk.clone())
            .materialize();

        left.insert("a".to_string(), 10);
        assert_eq!(joined.get_value(&"a".to_string()), None);

        right.insert("10".to_string(), 99);
        assert_eq!(joined.get_value(&"a".to_string()), Some(10));
    }

    #[test]
    fn left_semi_join_by_preserves_right_batch() {
        let left = CellMap::<String, i32>::new();
        left.insert("a".to_string(), 1);
        left.insert("b".to_string(), 2);

        let right = CellMap::<String, bool>::new();
        let joined = left
            .clone()
            .left_semi_join_by(right.clone(), |_, lv| lv.to_string(), |rk, _| rk.clone())
            .materialize();

        let (tx, rx) = mpsc::channel::<MapDiff<String, i32>>();
        let _guard = joined.subscribe_diffs(move |diff| {
            let _ = tx.send(diff.clone());
        });

        right.insert_many(vec![("1".to_string(), true), ("2".to_string(), true)]);

        let seen: Vec<_> = rx.try_iter().collect();
        assert_eq!(seen.len(), 2);
        match seen.last().expect("last diff") {
            MapDiff::Batch { changes } => assert_eq!(changes.len(), 2),
            _ => panic!("expected batch diff from left_semi_join_by"),
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
    fn left_semi_join_fk_keeps_matched_users() {
        let users = CellMap::<String, User>::new();
        let posts = CellMap::<String, Post>::new();
        let joined = users.clone().left_semi_join_fk(posts.clone()).materialize();

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

        assert_eq!(joined.entries().get().len(), 0);

        posts.insert(
            "p1".to_string(),
            Post {
                user_id: UserId("u1".to_string()),
                title: "Hello".to_string(),
            },
        );

        assert_eq!(joined.entries().get().len(), 1);
        assert_eq!(
            joined.get_value(&"u1".to_string()),
            Some(User {
                name: "Alice".to_string()
            })
        );
        assert_eq!(joined.get_value(&"u2".to_string()), None);
    }

    #[test]
    fn left_semi_join_fk_reacts_to_post_removal() {
        let users = CellMap::<String, User>::new();
        let posts = CellMap::<String, Post>::new();
        let joined = users.clone().left_semi_join_fk(posts.clone()).materialize();

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
        assert_eq!(joined.entries().get().len(), 1);

        posts.remove(&"p1".to_string());
        assert_eq!(joined.entries().get().len(), 0);
    }
}
