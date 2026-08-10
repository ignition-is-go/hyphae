//! Left-join plan nodes implementing [`MapQuery`].
//!
//! `left_join`, `left_join_fk`, and `left_join_by` build uncompiled plan nodes
//! that compose with other [`MapQuery`] operators. Call
//! [`MapQuery::materialize`] to compile a plan into a subscribable
//! [`CellMap`](crate::CellMap).

use std::{hash::Hash, marker::PhantomData};

use crate::{
    map_query::{MapDiffSink, MapQuery, MapQueryInstall},
    subscription::SubscriptionGuard,
    traits::{
        CellValue, ForeignKeyRelation, IdFor,
        collections::internal::join_runtime::install_keyed_join_runtime_via_query,
    },
};

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
    FR: Fn(&RK, &RV) -> JK + Send + Sync + 'static,
{
    pub(crate) left: L,
    pub(crate) right: R,
    pub(crate) left_key: FL,
    pub(crate) right_key: FR,
    #[allow(clippy::type_complexity)]
    pub(crate) _types: PhantomData<fn() -> (LK, LV, RK, RV, JK)>,
}

impl<L, R, LK, LV, RK, RV, JK, FL, FR> MapQueryInstall<LK, (LV, Vec<RV>)>
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
    FR: Fn(&RK, &RV) -> JK + Send + Sync + 'static,
{
    fn install(self, sink: MapDiffSink<LK, (LV, Vec<RV>)>) -> Vec<SubscriptionGuard> {
        install_keyed_join_runtime_via_query::<LK, LV, RK, RV, JK, (LV, Vec<RV>), _, _, _, _, _>(
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
    FR: Fn(&RK, &RV) -> JK + Send + Sync + 'static,
{
    type Key = LK;
    type Value = (LV, Vec<RV>);
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
    fn left_join<R, RV>(self, right: R) -> impl MapQuery<Key = K, Value = (V, Vec<RV>)>
    where
        R: MapQuery<Key = K, Value = RV>,
        RV: CellValue,
    {
        LeftJoinPlan {
            left: self,
            right,
            left_key: |k: &K, _: &V| k.clone(),
            right_key: |k: &K, _: &RV| k.clone(),
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
    fn left_join_fk<Rel, R>(self, right: R) -> impl MapQuery<Key = K, Value = (V, Vec<Rel::Child>)>
    where
        Rel: ForeignKeyRelation,
        R: MapQuery<Value = Rel::Child>,
        Rel::ForeignKey: IdFor<Rel::Parent, MapKey = K>,
    {
        LeftJoinPlan {
            left: self,
            right,
            left_key: |k: &K, _: &V| Some(k.clone()),
            right_key: |_: &R::Key, rv: &Rel::Child| {
                Rel::foreign_key(rv).map(|foreign_key| foreign_key.map_key())
            },
            _types: PhantomData,
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
    ) -> impl MapQuery<Key = K, Value = (V, Vec<RV>)>
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
            right_key,
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
            Some(post.user_id.clone())
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
}
