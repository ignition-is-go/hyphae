//! A 66,000-row batch through two sharded joins.
//!
//! This is its own test binary because the batch is one long settle. In a
//! shared process it would hold up every other test's writes for its
//! duration.
#![cfg(feature = "scheduler")]

use hyphae::{CellMap, ForeignKeyRelation, IdFor, IdType, LeftJoinExt, MapQuery};

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
