use std::hash::Hash;

use crate::{
    cell::CellImmutable,
    cell_map::CellMap,
    traits::{
        CellValue, HasForeignKey, IdFor, collections::internal::join_runtime::run_join_runtime,
        reactive_keys::ReactiveKeys, reactive_map::ReactiveMap,
    },
};

type LeftJoinResult<L, R> = CellMap<
    <L as ReactiveKeys>::Key,
    (<L as ReactiveMap>::Value, Vec<<R as ReactiveMap>::Value>),
    CellImmutable,
>;

pub trait LeftJoinExt: ReactiveMap {
    /// Left join on equal map keys.
    ///
    /// Every left row produces exactly one output row. Right matches are collected into a `Vec`.
    /// An empty `Vec` means no matching right rows were found.
    fn left_join<R>(&self, right: &R) -> LeftJoinResult<Self, R>
    where
        R: ReactiveMap<Key = Self::Key>;

    /// Left join using foreign key relationship.
    ///
    /// Joins on the left map key matching the right value's foreign key.
    /// Every left row produces exactly one output row. Right matches are collected into a `Vec`.
    /// An empty `Vec` means no matching right rows were found.
    fn left_join_fk<R>(&self, right: &R) -> LeftJoinResult<Self, R>
    where
        R: ReactiveMap,
        R::Value: HasForeignKey<Self::Value>,
        <<R::Value as HasForeignKey<Self::Value>>::ForeignKey as IdFor<Self::Value>>::MapKey:
            Into<Self::Key>;

    /// Left join using explicit key extractors.
    ///
    /// `left_key` and `right_key` extract the join key from each side.
    /// Every left row produces exactly one output row. Right matches are collected into a `Vec`.
    /// An empty `Vec` means no matching right rows were found.
    fn left_join_by<R, JK, FL, FR>(
        &self,
        right: &R,
        left_key: FL,
        right_key: FR,
    ) -> LeftJoinResult<Self, R>
    where
        R: ReactiveMap,
        JK: Hash + Eq + CellValue,
        FL: Fn(&Self::Key, &Self::Value) -> JK + Send + Sync + 'static,
        FR: Fn(&R::Key, &R::Value) -> JK + Send + Sync + 'static;
}

impl<L: ReactiveMap> LeftJoinExt for L {
    fn left_join<R>(&self, right: &R) -> LeftJoinResult<Self, R>
    where
        R: ReactiveMap<Key = Self::Key>,
    {
        run_join_runtime(
            self,
            right,
            "left_join",
            |k: &Self::Key, _: &Self::Value| k.clone(),
            |k: &Self::Key, _: &R::Value| k.clone(),
            |left_k, left_v, rights| {
                let right_values: Vec<R::Value> = rights.iter().map(|(_, rv)| rv.clone()).collect();
                vec![(left_k.clone(), (left_v.clone(), right_values))]
            },
        )
    }

    fn left_join_fk<R>(&self, right: &R) -> LeftJoinResult<Self, R>
    where
        R: ReactiveMap,
        R::Value: HasForeignKey<Self::Value>,
        <<R::Value as HasForeignKey<Self::Value>>::ForeignKey as IdFor<Self::Value>>::MapKey:
            Into<Self::Key>,
    {
        self.left_join_by(
            right,
            |k: &Self::Key, _: &Self::Value| k.clone(),
            |_: &R::Key, rv: &R::Value| rv.fk().map_key().into(),
        )
    }

    fn left_join_by<R, JK, FL, FR>(
        &self,
        right: &R,
        left_key: FL,
        right_key: FR,
    ) -> LeftJoinResult<Self, R>
    where
        R: ReactiveMap,
        JK: Hash + Eq + CellValue,
        FL: Fn(&Self::Key, &Self::Value) -> JK + Send + Sync + 'static,
        FR: Fn(&R::Key, &R::Value) -> JK + Send + Sync + 'static,
    {
        run_join_runtime(
            self,
            right,
            "left_join_by",
            left_key,
            right_key,
            |left_k, left_v, rights| {
                let right_values: Vec<R::Value> = rights.iter().map(|(_, rv)| rv.clone()).collect();
                vec![(left_k.clone(), (left_v.clone(), right_values))]
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
    fn left_join_keeps_unmatched_left_rows() {
        let left = CellMap::<String, i32>::new();
        let right = CellMap::<String, i32>::new();
        let joined = left.left_join(&right);

        left.insert("a".to_string(), 1);
        assert_eq!(joined.get_value(&"a".to_string()), Some((1, vec![])));
    }

    #[test]
    fn left_join_pairs_matched_rows() {
        let left = CellMap::<String, i32>::new();
        let right = CellMap::<String, i32>::new();
        let joined = left.left_join(&right);

        left.insert("a".to_string(), 1);
        right.insert("a".to_string(), 10);
        assert_eq!(joined.get_value(&"a".to_string()), Some((1, vec![10])));
    }

    #[test]
    fn left_join_reacts_to_right_addition() {
        let left = CellMap::<String, i32>::new();
        let right = CellMap::<String, i32>::new();
        let joined = left.left_join(&right);

        left.insert("a".to_string(), 1);
        assert_eq!(joined.get_value(&"a".to_string()), Some((1, vec![])));

        right.insert("a".to_string(), 10);
        assert_eq!(joined.get_value(&"a".to_string()), Some((1, vec![10])));
    }

    #[test]
    fn left_join_reacts_to_right_removal() {
        let left = CellMap::<String, i32>::new();
        let right = CellMap::<String, i32>::new();
        let joined = left.left_join(&right);

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
        let joined = left.left_join(&right);

        left.insert("a".to_string(), 1);
        right.insert("a".to_string(), 10);
        assert_eq!(joined.entries().get().len(), 1);

        left.remove(&"a".to_string());
        assert_eq!(joined.entries().get().len(), 0);
    }

    #[test]
    fn left_join_by_collects_multiple_right_matches() {
        let left = CellMap::<String, (String, i32)>::new();
        let right = CellMap::<String, (String, i32)>::new();
        let joined = left.left_join_by(&right, |_, lv| lv.0.clone(), |_, rv| rv.0.clone());

        left.insert("l1".to_string(), ("g1".to_string(), 10));
        right.insert("r1".to_string(), ("g1".to_string(), 5));
        right.insert("r2".to_string(), ("g1".to_string(), 7));

        let val = joined.get_value(&"l1".to_string());
        assert!(val.is_some());
        let (left_val, right_vals) = val.unwrap();
        assert_eq!(left_val, ("g1".to_string(), 10));
        assert_eq!(right_vals.len(), 2);
    }

    #[test]
    fn left_join_by_keeps_unmatched_with_empty_vec() {
        let left = CellMap::<String, (String, i32)>::new();
        let right = CellMap::<String, (String, i32)>::new();
        let joined = left.left_join_by(&right, |_, lv| lv.0.clone(), |_, rv| rv.0.clone());

        left.insert("l1".to_string(), ("g1".to_string(), 10));

        let val = joined.get_value(&"l1".to_string());
        assert!(val.is_some());
        let (left_val, right_vals) = val.unwrap();
        assert_eq!(left_val, ("g1".to_string(), 10));
        assert_eq!(right_vals.len(), 0);
    }

    #[test]
    fn left_join_by_preserves_right_batch() {
        let left = CellMap::<String, (String, i32)>::new();
        left.insert("l1".to_string(), ("g1".to_string(), 10));

        let right = CellMap::<String, (String, i32)>::new();
        let joined = left.left_join_by(&right, |_, lv| lv.0.clone(), |_, rv| rv.0.clone());

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
        match seen.last().expect("last diff") {
            MapDiff::Batch { changes } => assert!(!changes.is_empty()),
            _ => panic!("expected batch diff from left_join_by"),
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
    fn left_join_fk_keeps_unmatched_with_empty_vec() {
        let users = CellMap::<String, User>::new();
        let posts = CellMap::<String, Post>::new();
        let joined = users.left_join_fk(&posts);

        users.insert(
            "u1".to_string(),
            User {
                name: "Alice".to_string(),
            },
        );

        let val = joined.get_value(&"u1".to_string());
        assert!(val.is_some());
        let (user, posts) = val.unwrap();
        assert_eq!(user.name, "Alice");
        assert_eq!(posts.len(), 0);
    }

    #[test]
    fn left_join_fk_collects_matching_posts() {
        let users = CellMap::<String, User>::new();
        let posts = CellMap::<String, Post>::new();
        let joined = users.left_join_fk(&posts);

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
        assert!(val.is_some());
        let (_, matched_posts) = val.unwrap();
        assert_eq!(matched_posts.len(), 2);
    }
}
