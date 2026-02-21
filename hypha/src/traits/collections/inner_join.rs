use std::hash::Hash;

use crate::{
    cell::CellImmutable,
    cell_map::CellMap,
    traits::{CellValue, HasForeignKey, IdFor, collections::internal::join_runtime::run_join_runtime},
};

pub trait InnerJoinExt<K, V>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    /// Inner join on equal map keys.
    ///
    /// Pairs left and right rows that share the same map key.
    /// Produces one output row per match, keyed by the shared key.
    /// Unmatched rows from either side are excluded.
    fn inner_join<RV, RM>(
        &self,
        right: &CellMap<K, RV, RM>,
    ) -> CellMap<K, (V, RV), CellImmutable>
    where
        RV: CellValue,
        RM: Clone + Send + Sync + 'static;

    /// Inner join using foreign key relationship.
    ///
    /// Joins on the left map key matching the right value's foreign key.
    /// Produces one output row per matching (left, right) pair, keyed by `(K, RK)`.
    /// Unmatched rows from either side are excluded.
    fn inner_join_fk<RK, RV, RM>(
        &self,
        right: &CellMap<RK, RV, RM>,
    ) -> CellMap<(K, RK), (V, RV), CellImmutable>
    where
        RK: Hash + Eq + CellValue,
        RV: CellValue + HasForeignKey<V>,
        <<RV as HasForeignKey<V>>::ForeignKey as IdFor<V>>::MapKey: Into<K>;

    /// Inner join using explicit key extractors.
    ///
    /// `left_key` and `right_key` extract the join key from each side.
    /// Produces one output row per matching (left, right) pair, keyed by `(K, RK)`.
    /// Unmatched rows from either side are excluded.
    fn inner_join_by<RK, RV, RM, JK, FL, FR>(
        &self,
        right: &CellMap<RK, RV, RM>,
        left_key: FL,
        right_key: FR,
    ) -> CellMap<(K, RK), (V, RV), CellImmutable>
    where
        RK: Hash + Eq + CellValue,
        RV: CellValue,
        JK: Hash + Eq + CellValue,
        FL: Fn(&K, &V) -> JK + Send + Sync + 'static,
        FR: Fn(&RK, &RV) -> JK + Send + Sync + 'static;
}

impl<K, V, M> InnerJoinExt<K, V> for CellMap<K, V, M>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    fn inner_join<RV, RM>(
        &self,
        right: &CellMap<K, RV, RM>,
    ) -> CellMap<K, (V, RV), CellImmutable>
    where
        RV: CellValue,
        RM: Clone + Send + Sync + 'static,
    {
        run_join_runtime(
            self,
            right,
            "inner_join",
            |k: &K, _: &V| k.clone(),
            |k: &K, _: &RV| k.clone(),
            |left_k, left_v, rights| {
                rights
                    .iter()
                    .map(|(_, rv)| (left_k.clone(), (left_v.clone(), rv.clone())))
                    .collect()
            },
        )
    }

    fn inner_join_fk<RK, RV, RM>(
        &self,
        right: &CellMap<RK, RV, RM>,
    ) -> CellMap<(K, RK), (V, RV), CellImmutable>
    where
        RK: Hash + Eq + CellValue,
        RV: CellValue + HasForeignKey<V>,
        <<RV as HasForeignKey<V>>::ForeignKey as IdFor<V>>::MapKey: Into<K>,
    {
        self.inner_join_by(
            right,
            |k: &K, _: &V| k.clone(),
            |_: &RK, rv: &RV| rv.fk().map_key().into(),
        )
    }

    fn inner_join_by<RK, RV, RM, JK, FL, FR>(
        &self,
        right: &CellMap<RK, RV, RM>,
        left_key: FL,
        right_key: FR,
    ) -> CellMap<(K, RK), (V, RV), CellImmutable>
    where
        RK: Hash + Eq + CellValue,
        RV: CellValue,
        JK: Hash + Eq + CellValue,
        FL: Fn(&K, &V) -> JK + Send + Sync + 'static,
        FR: Fn(&RK, &RV) -> JK + Send + Sync + 'static,
    {
        run_join_runtime(
            self,
            right,
            "inner_join_by",
            left_key,
            right_key,
            |left_k, left_v, rights| {
                rights
                    .iter()
                    .map(|(rk, rv)| {
                        ((left_k.clone(), rk.clone()), (left_v.clone(), rv.clone()))
                    })
                    .collect()
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;
    use crate::{MapDiff, traits::{Gettable, HasForeignKey, IdFor, IdType}};

    #[test]
    fn inner_join_pairs_on_equal_keys() {
        let left = CellMap::<String, i32>::new();
        let right = CellMap::<String, i32>::new();
        let joined = left.inner_join(&right);

        left.insert("a".to_string(), 1);
        assert_eq!(joined.entries().get().len(), 0);

        right.insert("a".to_string(), 10);
        assert_eq!(joined.get_value(&"a".to_string()), Some((1, 10)));
    }

    #[test]
    fn inner_join_excludes_unmatched() {
        let left = CellMap::<String, i32>::new();
        let right = CellMap::<String, i32>::new();
        let joined = left.inner_join(&right);

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
        let joined = left.inner_join(&right);

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
        let joined = left.inner_join(&right);

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
        let joined = left.inner_join_by(
            &right,
            |_, lv| lv.0.clone(),
            |_, rv| rv.0.clone(),
        );

        left.insert("l1".to_string(), ("g1".to_string(), 10));
        right.insert("r1".to_string(), ("g1".to_string(), 5));

        let key = ("l1".to_string(), "r1".to_string());
        let val = joined.get_value(&key);
        assert_eq!(
            val,
            Some((("g1".to_string(), 10), ("g1".to_string(), 5)))
        );
    }

    #[test]
    fn inner_join_by_handles_one_to_many() {
        let left = CellMap::<String, (String, i32)>::new();
        let right = CellMap::<String, (String, i32)>::new();
        let joined = left.inner_join_by(
            &right,
            |_, lv| lv.0.clone(),
            |_, rv| rv.0.clone(),
        );

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
        let joined = left.inner_join_by(
            &right,
            |_, lv| lv.0.clone(),
            |_, rv| rv.0.clone(),
        );

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
        let joined = users.inner_join_fk(&posts);

        users.insert("u1".to_string(), User { name: "Alice".to_string() });
        posts.insert("p1".to_string(), Post {
            user_id: UserId("u1".to_string()),
            title: "Hello".to_string(),
        });

        let key = ("u1".to_string(), "p1".to_string());
        let val = joined.get_value(&key);
        assert_eq!(
            val,
            Some((
                User { name: "Alice".to_string() },
                Post { user_id: UserId("u1".to_string()), title: "Hello".to_string() },
            ))
        );
    }

    #[test]
    fn inner_join_fk_handles_one_to_many() {
        let users = CellMap::<String, User>::new();
        let posts = CellMap::<String, Post>::new();
        let joined = users.inner_join_fk(&posts);

        users.insert("u1".to_string(), User { name: "Alice".to_string() });
        posts.insert("p1".to_string(), Post {
            user_id: UserId("u1".to_string()),
            title: "Hello".to_string(),
        });
        posts.insert("p2".to_string(), Post {
            user_id: UserId("u1".to_string()),
            title: "World".to_string(),
        });

        assert_eq!(joined.entries().get().len(), 2);
    }

    #[test]
    fn inner_join_fk_excludes_unmatched() {
        let users = CellMap::<String, User>::new();
        let posts = CellMap::<String, Post>::new();
        let joined = users.inner_join_fk(&posts);

        users.insert("u1".to_string(), User { name: "Alice".to_string() });
        posts.insert("p1".to_string(), Post {
            user_id: UserId("u2".to_string()),
            title: "Orphan".to_string(),
        });

        assert_eq!(joined.entries().get().len(), 0);
    }
}
