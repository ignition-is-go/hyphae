use std::hash::Hash;

use crate::{
    cell::CellImmutable,
    cell_map::CellMap,
    traits::{
        CellValue, HasForeignKey, IdFor, collections::internal::join_runtime::run_join_runtime,
        reactive_map::ReactiveMap,
    },
};

pub trait LeftSemiJoinExt: ReactiveMap {
    /// Left semi join on equal map keys.
    ///
    /// Keeps left rows where a right row with the same key exists.
    /// Unmatched left rows are excluded. Output contains only left data.
    fn left_semi_join<R>(&self, right: &R) -> CellMap<Self::Key, Self::Value, CellImmutable>
    where
        R: ReactiveMap<Key = Self::Key>;

    /// Left semi join using foreign key relationship.
    ///
    /// Joins on the left map key matching the right value's foreign key.
    /// Keeps left rows that have at least one matching right row.
    /// Unmatched left rows are excluded. Output contains only left data.
    fn left_semi_join_fk<R>(&self, right: &R) -> CellMap<Self::Key, Self::Value, CellImmutable>
    where
        R: ReactiveMap,
        R::Value: HasForeignKey<Self::Value>,
        <<R::Value as HasForeignKey<Self::Value>>::ForeignKey as IdFor<Self::Value>>::MapKey:
            Into<Self::Key>;

    /// Left semi join using explicit key extractors.
    ///
    /// `left_key` and `right_key` extract the join key from each side.
    /// Keeps left rows that have at least one matching right row.
    /// Unmatched left rows are excluded. Output contains only left data.
    fn left_semi_join_by<R, JK, FL, FR>(
        &self,
        right: &R,
        left_key: FL,
        right_key: FR,
    ) -> CellMap<Self::Key, Self::Value, CellImmutable>
    where
        R: ReactiveMap,
        JK: Hash + Eq + CellValue,
        FL: Fn(&Self::Key, &Self::Value) -> JK + Send + Sync + 'static,
        FR: Fn(&R::Key, &R::Value) -> JK + Send + Sync + 'static;
}

impl<L: ReactiveMap> LeftSemiJoinExt for L {
    fn left_semi_join<R>(&self, right: &R) -> CellMap<Self::Key, Self::Value, CellImmutable>
    where
        R: ReactiveMap<Key = Self::Key>,
    {
        run_join_runtime(
            self,
            right,
            "left_semi_join",
            |k: &Self::Key, _: &Self::Value| k.clone(),
            |k: &Self::Key, _: &R::Value| k.clone(),
            |left_k, left_v, rights| {
                if rights.is_empty() {
                    Vec::new()
                } else {
                    vec![(left_k.clone(), left_v.clone())]
                }
            },
        )
    }

    fn left_semi_join_fk<R>(&self, right: &R) -> CellMap<Self::Key, Self::Value, CellImmutable>
    where
        R: ReactiveMap,
        R::Value: HasForeignKey<Self::Value>,
        <<R::Value as HasForeignKey<Self::Value>>::ForeignKey as IdFor<Self::Value>>::MapKey:
            Into<Self::Key>,
    {
        self.left_semi_join_by(
            right,
            |k: &Self::Key, _: &Self::Value| k.clone(),
            |_: &R::Key, rv: &R::Value| rv.fk().map_key().into(),
        )
    }

    fn left_semi_join_by<R, JK, FL, FR>(
        &self,
        right: &R,
        left_key: FL,
        right_key: FR,
    ) -> CellMap<Self::Key, Self::Value, CellImmutable>
    where
        R: ReactiveMap,
        JK: Hash + Eq + CellValue,
        FL: Fn(&Self::Key, &Self::Value) -> JK + Send + Sync + 'static,
        FR: Fn(&R::Key, &R::Value) -> JK + Send + Sync + 'static,
    {
        run_join_runtime(
            self,
            right,
            "left_semi_join_by",
            left_key,
            right_key,
            |left_k, left_v, rights| {
                if rights.is_empty() {
                    Vec::new()
                } else {
                    vec![(left_k.clone(), left_v.clone())]
                }
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;
    use crate::{
        MapDiff,
        traits::{Gettable, HasForeignKey, IdFor, IdType},
    };

    #[test]
    fn left_semi_join_keeps_matched_rows() {
        let left = CellMap::<String, i32>::new();
        let right = CellMap::<String, bool>::new();
        let joined = left.left_semi_join(&right);

        left.insert("a".to_string(), 1);
        assert_eq!(joined.entries().get().len(), 0);

        right.insert("a".to_string(), true);
        assert_eq!(joined.get_value(&"a".to_string()), Some(1));
    }

    #[test]
    fn left_semi_join_drops_unmatched_rows() {
        let left = CellMap::<String, i32>::new();
        let right = CellMap::<String, bool>::new();
        let joined = left.left_semi_join(&right);

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
        let joined = left.left_semi_join(&right);

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
        let joined = left.left_semi_join_by(&right, |_, lv| lv.to_string(), |rk, _| rk.clone());

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
        let joined = left.left_semi_join_by(&right, |_, lv| lv.to_string(), |rk, _| rk.clone());

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
        let joined = users.left_semi_join_fk(&posts);

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
        let joined = users.left_semi_join_fk(&posts);

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
