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

#[test]
fn coordinated_two_join_region_tracks_every_root() {
    let left = CellMap::<u64, (u64, i32)>::new();
    let right1 = CellMap::<u64, (u64, i32)>::new();
    let right2 = CellMap::<u64, (u64, i32)>::new();
    let output = left
        .clone()
        .left_join_by(right1.clone(), |_, row| row.0, |_, row| row.0)
        .map_joined_values(|_, left, matches| {
            (
                left.0,
                left.1 + matches.iter().map(|(_, row)| row.1).sum::<i32>(),
            )
        })
        .left_join_by(right2.clone(), |_, row| row.0, |_, row| row.0)
        .map_joined_values(|_, middle, matches| {
            middle.1 + matches.iter().map(|(_, row)| row.1).sum::<i32>()
        })
        .materialize();

    left.insert(1, (7, 10));
    assert_eq!(output.get_value(&1), Some(10));

    right1.insert(11, (7, 3));
    assert_eq!(output.get_value(&1), Some(13));

    right2.insert(21, (7, 5));
    assert_eq!(output.get_value(&1), Some(18));

    right1.remove(&11);
    assert_eq!(output.get_value(&1), Some(15));

    right2.remove(&21);
    assert_eq!(output.get_value(&1), Some(10));

    left.remove(&1);
    assert_eq!(output.get_value(&1), None);
}

#[test]
fn coordinated_two_join_region_preserves_batched_updates() {
    let left = CellMap::<u64, (u64, i32)>::new();
    let right1 = CellMap::<u64, (u64, i32)>::new();
    let right2 = CellMap::<u64, (u64, i32)>::new();
    let output = left
        .clone()
        .left_join_by(right1.clone(), |_, row| row.0, |_, row| row.0)
        .map_joined_values(|_, left, matches| {
            (
                left.0,
                left.1 + matches.iter().map(|(_, row)| row.1).sum::<i32>(),
            )
        })
        .left_join_by(right2.clone(), |_, row| row.0, |_, row| row.0)
        .map_joined_values(|_, middle, matches| {
            middle.1 + matches.iter().map(|(_, row)| row.1).sum::<i32>()
        })
        .materialize();

    left.insert_many(vec![(1, (7, 10)), (2, (8, 20))]);
    right1.insert_many(vec![(11, (7, 3)), (12, (8, 4))]);
    right2.insert_many(vec![(21, (7, 5)), (22, (8, 6))]);

    assert_eq!(output.get_value(&1), Some(18));
    assert_eq!(output.get_value(&2), Some(30));
}

#[test]
fn coordinated_two_join_region_repartitions_updates_between_joins() {
    let left = CellMap::<u64, (u64, i32)>::new();
    let right1 = CellMap::<u64, (u64, u64, i32)>::new();
    let right2 = CellMap::<u64, (u64, i32)>::new();
    let output = left
        .clone()
        .left_join_by(right1.clone(), |_, row| row.0, |_, row| row.0)
        .map_joined_values(|_, left, matches| {
            let next_relation = matches.first().map_or(0, |(_, row)| row.1);
            let subtotal = left.1 + matches.iter().map(|(_, row)| row.2).sum::<i32>();
            (next_relation, subtotal)
        })
        .left_join_by(right2.clone(), |_, row| row.0, |_, row| row.0)
        .map_joined_values(|_, middle, matches| {
            middle.1 + matches.iter().map(|(_, row)| row.1).sum::<i32>()
        })
        .materialize();

    right2.insert_many(vec![(21, (20, 5)), (31, (30, 7))]);
    right1.insert(11, (10, 20, 3));
    left.insert(1, (10, 100));
    assert_eq!(output.get_value(&1), Some(108));

    // Updating the first relation changes the intermediate join key. The
    // row must leave its old second-stage shard and enter the new one.
    right1.insert(11, (10, 30, 4));
    assert_eq!(output.get_value(&1), Some(111));

    // Moving both sides of the first join exercises route removal and
    // reinsertion while preserving the final map key.
    right1.insert(11, (40, 20, 9));
    assert_eq!(output.get_value(&1), Some(100));
    left.insert(1, (40, 100));
    assert_eq!(output.get_value(&1), Some(114));
}

#[test]
fn coordinated_two_join_matches_reference_across_mixed_root_updates() {
    use std::collections::HashMap;

    let left = CellMap::<u64, (u64, i64)>::new();
    let right1 = CellMap::<u64, (u64, i64)>::new();
    let right2 = CellMap::<u64, (u64, i64)>::new();
    let output = left
        .clone()
        .left_join_by(right1.clone(), |_, row| row.0, |_, row| row.0)
        .map_joined_values(|_, left, matches| {
            let subtotal = left.1 + matches.iter().map(|(_, row)| row.1).sum::<i64>();
            (u64::try_from(subtotal.rem_euclid(8)).unwrap_or(0), subtotal)
        })
        .left_join_by(right2.clone(), |_, row| row.0, |_, row| row.0)
        .map_joined_values(|_, middle, matches| {
            middle.1 + matches.iter().map(|(_, row)| row.1).sum::<i64>()
        })
        .materialize();

    let mut left_reference = HashMap::new();
    let mut right1_reference = HashMap::new();
    let mut right2_reference = HashMap::new();
    let mut random = 0x9e37_79b9_7f4a_7c15_u64;

    for _ in 0..512 {
        random = random
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        let key = (random >> 16) % 16;
        let relation = (random >> 32) % 8;
        let value = i64::try_from((random >> 48) % 31).unwrap_or(0) - 15;
        match random % 6 {
            0 => {
                left.insert(key, (relation, value));
                left_reference.insert(key, (relation, value));
            }
            1 => {
                right1.insert(key, (relation, value));
                right1_reference.insert(key, (relation, value));
            }
            2 => {
                right2.insert(key, (relation, value));
                right2_reference.insert(key, (relation, value));
            }
            3 => {
                left.remove(&key);
                left_reference.remove(&key);
            }
            4 => {
                right1.remove(&key);
                right1_reference.remove(&key);
            }
            _ => {
                right2.remove(&key);
                right2_reference.remove(&key);
            }
        }

        for candidate in 0..16 {
            let expected = left_reference.get(&candidate).map(|left_row| {
                let subtotal = left_row.1
                    + right1_reference
                        .values()
                        .filter(|row| row.0 == left_row.0)
                        .map(|row| row.1)
                        .sum::<i64>();
                let second_relation = u64::try_from(subtotal.rem_euclid(8)).unwrap_or(0);
                subtotal
                    + right2_reference
                        .values()
                        .filter(|row| row.0 == second_relation)
                        .map(|row| row.1)
                        .sum::<i64>()
            });
            assert_eq!(output.get_value(&candidate), expected);
        }
    }
}

#[test]
fn joined_projection_preserves_right_insertion_order() {
    let left = CellMap::<u64, u64>::new();
    let right = CellMap::<u64, u64>::new();
    let output = left
        .clone()
        .left_join_by(right.clone(), |_, group| *group, |_, group| *group)
        .map_joined_values(|_, _, matches| matches.iter().map(|(key, _)| *key).collect::<Vec<_>>())
        .materialize();

    left.insert(1, 7);
    right.insert_many(vec![(30, 7), (10, 7), (20, 7)]);
    assert_eq!(output.get_value(&1), Some(vec![30, 10, 20]));

    right.insert(10, 7);
    assert_eq!(output.get_value(&1), Some(vec![30, 10, 20]));

    right.remove(&10);
    right.insert(10, 7);
    assert_eq!(output.get_value(&1), Some(vec![30, 20, 10]));
}

#[test]
fn right_changes_publish_impacted_left_rows_in_insertion_order() {
    let left = CellMap::<u64, u64>::new();
    let right = CellMap::<u64, u64>::new();
    let output = left
        .clone()
        .left_join_by(right.clone(), |_, group| *group, |_, group| *group)
        .map_joined_values(|_, _, matches| matches.len())
        .materialize();

    left.insert_many(vec![(9, 7), (3, 7), (7, 7)]);
    let (tx, rx) = mpsc::channel::<MapDiff<u64, usize>>();
    let _guard = output.subscribe_diffs(move |diff| {
        let _ = tx.send(diff.clone());
    });

    right.insert(1, 7);

    let diff = rx.try_iter().last();
    assert!(diff.is_some(), "right insert must publish");
    let keys: Vec<u64> = match diff {
        Some(MapDiff::Batch { changes }) => changes
            .iter()
            .filter_map(|change| match change {
                MapDiff::Update { key, .. } => Some(*key),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    };
    assert_eq!(keys, vec![9, 3, 7]);
}

#[cfg(feature = "scheduler")]
#[test]
fn large_parallel_join_batch_settles_synchronously_in_input_order() {
    const ROWS: u64 = 10_000;

    let left = CellMap::<u64, (u64, u64)>::new();
    let right1 = CellMap::<u64, (u64, u64)>::new();
    let right2 = CellMap::<u64, (u64, u64)>::new();
    right1.insert_many((0..8).map(|key| (key, (1, key))).collect());
    right2.insert_many((0..8).map(|key| (key, (1, key))).collect());

    let output = left
        .clone()
        .left_join_by(right1, |_, row| row.0, |_, row| row.0)
        .map_joined_values(|_, row, matches| {
            (
                row.0,
                row.1 + u64::try_from(matches.len()).unwrap_or(u64::MAX),
            )
        })
        .left_join_by(right2, |_, row| row.0, |_, row| row.0)
        .map_joined_values(|_, row, matches| {
            row.1 + u64::try_from(matches.len()).unwrap_or(u64::MAX)
        })
        .materialize();

    let (tx, rx) = mpsc::channel::<MapDiff<u64, u64>>();
    let _guard = output.subscribe_diffs(move |diff| {
        let _ = tx.send(diff.clone());
    });

    left.insert_many((0..ROWS).map(|key| (key, (1, key))).collect());

    assert_eq!(output.get_value(&(ROWS - 1)), Some(ROWS - 1 + 16));
    let diff = rx.try_iter().last();
    assert!(diff.is_some());
    let keys: Vec<u64> = match diff {
        Some(MapDiff::Batch { changes }) => changes
            .iter()
            .filter_map(|change| match change {
                MapDiff::Insert { key, .. } => Some(*key),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    };
    assert_eq!(keys, (0..ROWS).collect::<Vec<_>>());
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
fn left_join_fk_ignores_absent_foreign_keys() {
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
            user_id: UserId(String::new()),
            title: "Orphan".to_string(),
        },
    );
    assert!(matches!(joined.get_value(&"u1".to_string()), Some((_, rows)) if rows.is_empty()));
    posts.insert(
        "p1".to_string(),
        Post {
            user_id: UserId("u1".to_string()),
            title: "Attached".to_string(),
        },
    );
    assert!(matches!(joined.get_value(&"u1".to_string()), Some((_, rows)) if rows.len() == 1));
    posts.insert(
        "p1".to_string(),
        Post {
            user_id: UserId(String::new()),
            title: "Detached".to_string(),
        },
    );
    assert!(matches!(joined.get_value(&"u1".to_string()), Some((_, rows)) if rows.is_empty()));
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

#[test]
fn repeated_fk_relationship_keeps_distinct_projected_right_inputs() {
    let users = CellMap::<String, User>::new();
    let posts = CellMap::<String, Post>::new();
    let projected_a = posts.clone().map_values(|_, post| Post {
        user_id: post.user_id.clone(),
        title: format!("{}-a", post.title),
    });
    let projected_b = posts.clone().map_values(|_, post| Post {
        user_id: post.user_id.clone(),
        title: format!("{}-b", post.title),
    });
    let joined = users
        .clone()
        .left_join_fk::<UserPosts, _>(projected_a)
        .map_joined_values(|_, user, first_posts| {
            (
                user.clone(),
                first_posts.first().map(|(_, post)| post.title.clone()),
            )
        })
        .left_join_fk::<UserPosts, _>(projected_b)
        .map_joined_values(|_, first, second_posts| {
            (
                first.1.clone(),
                second_posts.first().map(|(_, post)| post.title.clone()),
            )
        })
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
            title: "First".to_string(),
        },
    );

    assert_eq!(
        joined.get_value(&"u1".to_string()),
        Some((Some("First-a".to_string()), Some("First-b".to_string())))
    );

    posts.insert(
        "p1".to_string(),
        Post {
            user_id: UserId("u1".to_string()),
            title: "Updated".to_string(),
        },
    );
    assert_eq!(
        joined.get_value(&"u1".to_string()),
        Some((Some("Updated-a".to_string()), Some("Updated-b".to_string())))
    );
}

#[test]
fn repeated_fk_relationship_reuses_index_and_updates_every_join() {
    let users = CellMap::<String, User>::new();
    let posts = CellMap::<String, Post>::new();
    let joined = users
        .clone()
        .left_join_fk::<UserPosts, _>(posts.clone())
        .map_joined_values(|_, user, first_posts| (user.clone(), first_posts.len()))
        .left_join_fk::<UserPosts, _>(posts.clone())
        .map_joined_values(|_, first, second_posts| (first.1, second_posts.len()))
        .materialize();

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
    posts.insert(
        "p1".to_string(),
        Post {
            user_id: UserId("u1".to_string()),
            title: "First".to_string(),
        },
    );

    assert_eq!(joined.get_value(&"u1".to_string()), Some((1, 1)));
    assert_eq!(joined.get_value(&"u2".to_string()), Some((0, 0)));

    posts.insert(
        "p1".to_string(),
        Post {
            user_id: UserId("u2".to_string()),
            title: "Moved".to_string(),
        },
    );

    assert_eq!(joined.get_value(&"u1".to_string()), Some((0, 0)));
    assert_eq!(joined.get_value(&"u2".to_string()), Some((1, 1)));

    posts.insert(
        "p1".to_string(),
        Post {
            user_id: UserId(String::new()),
            title: "Detached".to_string(),
        },
    );
    assert_eq!(joined.get_value(&"u1".to_string()), Some((0, 0)));
    assert_eq!(joined.get_value(&"u2".to_string()), Some((0, 0)));
}

#[test]
fn three_stage_typed_fk_region_updates_every_stage_from_one_root() {
    let users = CellMap::<String, User>::new();
    let posts = CellMap::<String, Post>::new();
    let joined = users
        .clone()
        .left_join_fk::<UserPosts, _>(posts.clone())
        .map_joined_values(|_, _, rows| rows.len())
        .left_join_fk::<UserPosts, _>(posts.clone())
        .map_joined_values(|_, first, rows| (*first, rows.len()))
        .left_join_fk::<UserPosts, _>(posts.clone())
        .map_joined_values(|_, counts, rows| (counts.0, counts.1, rows.len()))
        .materialize();

    users.insert(
        "u1".into(),
        User {
            name: "Alice".into(),
        },
    );
    users.insert("u2".into(), User { name: "Bob".into() });
    posts.insert(
        "p1".into(),
        Post {
            user_id: UserId("u1".into()),
            title: "First".into(),
        },
    );
    assert_eq!(joined.get_value(&"u1".to_string()), Some((1, 1, 1)));
    assert_eq!(joined.get_value(&"u2".to_string()), Some((0, 0, 0)));

    posts.insert(
        "p1".into(),
        Post {
            user_id: UserId("u2".into()),
            title: "Moved".into(),
        },
    );
    assert_eq!(joined.get_value(&"u1".to_string()), Some((0, 0, 0)));
    assert_eq!(joined.get_value(&"u2".to_string()), Some((1, 1, 1)));
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
fn repeated_optional_fk_relationship_tracks_some_none_transitions() {
    let users = CellMap::<String, User>::new();
    let posts = CellMap::<String, OptionalPost>::new();
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
    posts.insert(
        "p1".into(),
        OptionalPost {
            user_id: None,
            sequence: 1,
        },
    );
    assert_eq!(joined.get_value(&"u1".to_string()), Some((0, 0)));

    posts.insert(
        "p1".into(),
        OptionalPost {
            user_id: Some(UserId("u1".into())),
            sequence: 2,
        },
    );
    assert_eq!(joined.get_value(&"u1".to_string()), Some((1, 1)));

    posts.insert(
        "p1".into(),
        OptionalPost {
            user_id: None,
            sequence: 3,
        },
    );
    assert_eq!(joined.get_value(&"u1".to_string()), Some((0, 0)));
}

#[cfg(feature = "scheduler")]
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
