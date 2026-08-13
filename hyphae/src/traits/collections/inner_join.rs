//! Inner-join plan nodes implementing [`MapQuery`].
//!
//! `inner_join`, `inner_join_fk`, and `inner_join_by` build uncompiled plan
//! nodes that compose with other [`MapQuery`] operators. Call
//! [`MapQuery::materialize`] to compile a plan into a subscribable
//! [`CellMap`].

use std::{hash::Hash, marker::PhantomData};

use super::left_join::RelationPlan;

use crate::{
    map_query::{
        BuildQueryRuntime, MapQuery,
        properties::{ByMapKey, Many, PlanProperties, Repartition},
    },
    subscription::SubscriptionGuard,
    traits::{
        CellValue, ForeignKeyRelation, IdFor, OptionalRightKey, RequiredRightKey, RightJoinKey,
        collections::internal::join_runtime::{
            install_join_runtime_via_query, install_keyed_join_runtime_via_query,
        },
    },
};

/// Plan node for [`InnerJoinExt::inner_join`] (key-equal inner join).
///
/// Output rows are keyed by the shared map key with value `(LV, RV)`.
/// Not [`Clone`]: cloning a plan would silently duplicate join work; share by
/// materializing once.
pub struct InnerJoinByKeyPlan<L, R, K, LV, RV>
where
    L: MapQuery<Key = K, Value = LV>,
    R: MapQuery<Key = K, Value = RV>,
    K: Hash + Eq + CellValue,
    LV: CellValue,
    RV: CellValue,
{
    pub(crate) left: L,
    pub(crate) right: R,
    #[allow(clippy::type_complexity)]
    pub(crate) _types: PhantomData<fn() -> (K, LV, RV)>,
}

impl<L, R, K, LV, RV> PlanProperties for InnerJoinByKeyPlan<L, R, K, LV, RV>
where
    L: MapQuery<Key = K, Value = LV>,
    R: MapQuery<Key = K, Value = RV>,
    K: Hash + Eq + CellValue,
    LV: CellValue,
    RV: CellValue,
{
    type Cardinality = Many;
    type InputPartition = L::OutputPartition;
    type OutputPartition = ByMapKey<K>;
}

impl<L, R, K, LV, RV> BuildQueryRuntime<K, (LV, RV)> for InnerJoinByKeyPlan<L, R, K, LV, RV>
where
    L: MapQuery<Key = K, Value = LV>,
    R: MapQuery<Key = K, Value = RV>,
    K: Hash + Eq + CellValue,
    LV: CellValue,
    RV: CellValue,
{
    fn build_into(
        self,
        cx: &mut crate::map_query::compiler::CompileContext,
        sink: crate::map_query::BoxedMapDiffSink<K, (LV, RV)>,
    ) -> Vec<SubscriptionGuard> {
        install_keyed_join_runtime_via_query::<K, LV, K, RV, K, (LV, RV), _, _, _, _, _>(
            cx,
            self.left,
            self.right,
            |k: &K, _: &LV| k.clone(),
            RequiredRightKey(|k: &K, _: &RV| k.clone()),
            |_left_k: &K, left_v: &LV, rights: &[(K, RV)]| {
                rights
                    .first()
                    .map(|(_, right_v)| (left_v.clone(), right_v.clone()))
            },
            sink,
        )
    }
}

#[allow(private_bounds)]
impl<L, R, K, LV, RV> MapQuery for InnerJoinByKeyPlan<L, R, K, LV, RV>
where
    L: MapQuery<Key = K, Value = LV>,
    R: MapQuery<Key = K, Value = RV>,
    K: Hash + Eq + CellValue,
    LV: CellValue,
    RV: CellValue,
{
    type Key = K;
    type Value = (LV, RV);
}

/// Plan node for [`InnerJoinExt::inner_join_by`] and
/// [`InnerJoinExt::inner_join_fk`] (key-extractor inner joins).
///
/// Output rows are keyed by `(LK, RK)` with value `(LV, RV)`.
/// Not [`Clone`]: cloning a plan would silently duplicate join work; share by
/// materializing once.
pub struct InnerJoinByPairPlan<L, R, LK, LV, RK, RV, JK, FL, FR>
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
    for InnerJoinByPairPlan<L, R, LK, LV, RK, RV, JK, FL, FR>
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
    type Cardinality = Many;
    type InputPartition = L::OutputPartition;
    type OutputPartition = Repartition<(LK, RK)>;
}

impl<L, R, LK, LV, RK, RV, JK, FL, FR> BuildQueryRuntime<(LK, RK), (LV, RV)>
    for InnerJoinByPairPlan<L, R, LK, LV, RK, RV, JK, FL, FR>
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
        sink: crate::map_query::BoxedMapDiffSink<(LK, RK), (LV, RV)>,
    ) -> Vec<SubscriptionGuard> {
        install_join_runtime_via_query::<LK, LV, RK, RV, JK, (LK, RK), (LV, RV), _, _, _, _, _>(
            cx,
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
impl<L, R, LK, LV, RK, RV, JK, FL, FR> MapQuery
    for InnerJoinByPairPlan<L, R, LK, LV, RK, RV, JK, FL, FR>
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
    type Key = (LK, RK);
    type Value = (LV, RV);
}

/// Inner-join operators returning [`MapQuery`] plan nodes.
///
/// All three methods consume `self` and return uncompiled plan nodes; call
/// [`MapQuery::materialize`] on the result to obtain a subscribable
/// [`CellMap`](crate::CellMap).
pub trait InnerJoinExt<K, V>: MapQuery<Key = K, Value = V>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    /// Inner join on equal map keys.
    ///
    /// Pairs left and right rows that share the same map key.
    /// Produces one output row per match, keyed by the shared key.
    /// Unmatched rows from either side are excluded.
    fn inner_join<R, RV>(self, right: R) -> impl MapQuery<Key = K, Value = (V, RV)>
    where
        R: MapQuery<Key = K, Value = RV>,
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
    ///
    /// `Rel` is the relationship's semantic, partition, and reusable-index
    /// identity. `None` from its extractor denotes an absent optional right
    /// relationship and is omitted from the index. The key is converted into
    /// `IdFor<Rel::Parent>::MapKey`; it is independent of the left payload.
    #[allow(clippy::type_complexity)]
    fn inner_join_fk<Rel, R>(
        self,
        right: R,
    ) -> RelationPlan<
        InnerJoinByPairPlan<
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
        RelationPlan::<_, Rel>::new(InnerJoinByPairPlan {
            left: self,
            right,
            left_key: |k: &K, _: &V| k.clone(),
            right_key: OptionalRightKey(|_: &R::Key, rv: &Rel::Child| {
                Rel::foreign_key(rv).map(|foreign_key| foreign_key.map_key())
            }),
            _types: PhantomData,
        })
    }

    /// Inner join using explicit key extractors.
    ///
    /// `left_key` and `right_key` extract the join key from each side.
    /// This is an ad hoc escape hatch and carries no reusable typed relation
    /// identity; prefer the `*_join_fk` form for schema-owned relationships.
    /// Produces one output row per matching (left, right) pair, keyed by
    /// `(K, RK)`. Unmatched rows from either side are excluded.
    fn inner_join_by<R, RK, RV, JK, FL, FR>(
        self,
        right: R,
        left_key: FL,
        right_key: FR,
    ) -> impl MapQuery<Key = (K, RK), Value = (V, RV)>
    where
        R: MapQuery<Key = RK, Value = RV>,
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
            right_key: RequiredRightKey(right_key),
            _types: PhantomData,
        }
    }
}

impl<K, V, M> InnerJoinExt<K, V> for M
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
        CellMap, MapDiff, Materialize,
        traits::{ForeignKeyRelation, Gettable, IdFor, IdType},
    };

    #[test]
    fn inner_join_pairs_on_equal_keys() {
        let left = CellMap::<String, i32>::new();
        let right = CellMap::<String, i32>::new();
        let joined = left.clone().inner_join(right.clone()).materialize();

        left.insert("a".to_string(), 1);
        assert_eq!(joined.entries().materialize().get().len(), 0);

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

        assert_eq!(joined.entries().materialize().get().len(), 1);
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
        assert_eq!(joined.entries().materialize().get().len(), 1);

        right.remove(&"a".to_string());
        assert_eq!(joined.entries().materialize().get().len(), 0);
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

        assert_eq!(joined.entries().materialize().get().len(), 2);
    }

    #[test]
    fn inner_join_by_preserves_right_batch() {
        let left = CellMap::<String, (String, i32)>::new();
        left.insert("l1".to_string(), ("g1".to_string(), 10));

        let right = CellMap::<String, (String, i32)>::new();
        let joined = left
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
        assert!(matches!(
            seen.last(),
            Some(MapDiff::Batch { changes }) if changes.len() == 2
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
            (!post.user_id.0.is_empty()).then(|| post.user_id.clone())
        }
    }

    #[test]
    fn inner_join_fk_retains_relationship_partition_identity() {
        fn assert_relation_partition<P>(_: &P)
        where
            P: crate::map_query::properties::PlanProperties<
                    OutputPartition = crate::map_query::properties::ByRelation<UserPosts>,
                >,
        {
        }

        let users = CellMap::<String, User>::new();
        let posts = CellMap::<String, Post>::new();
        let plan = users.inner_join_fk::<UserPosts, _>(posts);
        assert_relation_partition(&plan);
    }

    #[test]
    fn inner_join_fk_pairs_on_foreign_key() {
        let users = CellMap::<String, User>::new();
        let posts = CellMap::<String, Post>::new();
        let joined = users
            .clone()
            .inner_join_fk::<UserPosts, _>(posts.clone())
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
    fn inner_join_fk_omits_absent_keys_and_handles_key_moves() {
        let users = CellMap::<String, User>::new();
        let posts = CellMap::<String, Post>::new();
        let joined = users
            .clone()
            .inner_join_fk::<UserPosts, _>(posts.clone())
            .materialize();
        for (key, name) in [("u1", "Alice"), ("u2", "Bob")] {
            users.insert(
                key.to_string(),
                User {
                    name: name.to_string(),
                },
            );
        }

        let post_key = "p1".to_string();
        posts.insert(
            post_key.clone(),
            Post {
                user_id: UserId(String::new()),
                title: "Orphan".to_string(),
            },
        );
        assert!(joined.entries().materialize().get().is_empty());

        posts.insert(
            post_key.clone(),
            Post {
                user_id: UserId("u1".to_string()),
                title: "Attached".to_string(),
            },
        );
        assert!(
            joined
                .get_value(&("u1".to_string(), post_key.clone()))
                .is_some()
        );

        posts.insert(
            post_key.clone(),
            Post {
                user_id: UserId("u2".to_string()),
                title: "Moved".to_string(),
            },
        );
        assert!(
            joined
                .get_value(&("u1".to_string(), post_key.clone()))
                .is_none()
        );
        assert!(
            joined
                .get_value(&("u2".to_string(), post_key.clone()))
                .is_some()
        );

        posts.insert(
            post_key,
            Post {
                user_id: UserId(String::new()),
                title: "Detached".to_string(),
            },
        );
        assert!(joined.entries().materialize().get().is_empty());
    }

    #[test]
    fn inner_join_fk_handles_one_to_many() {
        let users = CellMap::<String, User>::new();
        let posts = CellMap::<String, Post>::new();
        let joined = users
            .clone()
            .inner_join_fk::<UserPosts, _>(posts.clone())
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

        assert_eq!(joined.entries().materialize().get().len(), 2);
    }

    #[test]
    fn inner_join_fk_excludes_unmatched() {
        let users = CellMap::<String, User>::new();
        let posts = CellMap::<String, Post>::new();
        let joined = users
            .clone()
            .inner_join_fk::<UserPosts, _>(posts.clone())
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
                user_id: UserId("u2".to_string()),
                title: "Orphan".to_string(),
            },
        );

        assert_eq!(joined.entries().materialize().get().len(), 0);
    }

    #[derive(Debug, Clone, PartialEq)]
    struct OptionalPost {
        user_id: Option<UserId>,
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
    fn inner_join_fk_omits_absent_children_and_tracks_some_none_transitions() {
        let users = CellMap::<String, User>::new();
        let posts = CellMap::<String, OptionalPost>::new();
        let joined = users
            .clone()
            .inner_join_fk::<OptionalUserPosts, _>(posts.clone())
            .materialize();
        users.insert(
            "u1".into(),
            User {
                name: "Alice".into(),
            },
        );
        posts.insert("p1".into(), OptionalPost { user_id: None });
        assert!(joined.entries().materialize().get().is_empty());

        posts.insert(
            "p1".into(),
            OptionalPost {
                user_id: Some(UserId("u1".into())),
            },
        );
        assert!(joined.get_value(&("u1".into(), "p1".into())).is_some());

        posts.insert("p1".into(), OptionalPost { user_id: None });
        assert!(joined.entries().materialize().get().is_empty());
    }
}
