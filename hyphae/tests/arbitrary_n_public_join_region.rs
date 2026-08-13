#![allow(
    clippy::arithmetic_side_effects,
    clippy::missing_const_for_fn,
    clippy::trivially_copy_pass_by_ref
)]

use hyphae::{CellMap, LeftJoinExt, MapEntriesExt, MapQuery};

type Row = (u64, i64);
type RightRow = (u64, u64, i64);

fn project(_: &u64, left: &Row, matches: &[(u64, RightRow)]) -> Row {
    (
        matches.first().map_or(left.0, |(_, row)| row.1),
        left.1 + matches.iter().map(|(_, row)| row.2).sum::<i64>(),
    )
}

fn assert_query<Q: MapQuery<Key = u64, Value = Row>>(_: &Q) {}

#[test]
fn public_three_join_region_tracks_mixed_roots_and_rekeys() {
    let left = CellMap::<u64, Row>::new();
    let r1 = CellMap::<u64, RightRow>::new();
    let r2 = CellMap::<u64, RightRow>::new();
    let r3 = CellMap::<u64, RightRow>::new();

    let query = left
        .clone()
        .left_join_by(r1.clone(), |_, row| row.0, |_, row| row.0)
        .map_joined_values(project)
        .left_join_by(r2.clone(), |_, row| row.0, |_, row| row.0)
        .map_values(|_, joined| {
            (
                joined.1.first().map_or(joined.0.0, |row| row.1),
                joined.0.1 + joined.1.iter().map(|row| row.2).sum::<i64>(),
            )
        })
        .left_join_by(r3.clone(), |_, row| row.0, |_, row| row.0)
        .map_joined_values(project);
    assert_query(&query);
    let output = query.materialize();

    r1.insert(11, (1, 2, 10));
    r2.insert(12, (2, 3, 20));
    r3.insert(13, (3, 4, 30));
    left.insert(7, (1, 1));
    assert_eq!(output.get_value(&7), Some((4, 61)));

    r2.insert(12, (2, 9, 21));
    assert_eq!(output.get_value(&7), Some((9, 32)));
    r3.insert(14, (9, 5, 40));
    assert_eq!(output.get_value(&7), Some((5, 72)));
    r1.remove(&11);
    assert_eq!(output.get_value(&7), Some((1, 1)));
}

#[test]
fn public_eight_join_region_compiles_and_tracks_distant_right_roots() {
    let left = CellMap::<u64, Row>::new();
    let r1 = CellMap::<u64, RightRow>::new();
    let r2 = CellMap::<u64, RightRow>::new();
    let r3 = CellMap::<u64, RightRow>::new();
    let r4 = CellMap::<u64, RightRow>::new();
    let r5 = CellMap::<u64, RightRow>::new();
    let r6 = CellMap::<u64, RightRow>::new();
    let r7 = CellMap::<u64, RightRow>::new();
    let r8 = CellMap::<u64, RightRow>::new();

    let query = left
        .clone()
        .left_join_by(r1.clone(), |_, row| row.0, |_, row| row.0)
        .map_joined_values(project)
        .left_join_by(r2.clone(), |_, row| row.0, |_, row| row.0)
        .map_joined_values(project)
        .left_join_by(r3.clone(), |_, row| row.0, |_, row| row.0)
        .map_joined_values(project)
        .left_join_by(r4.clone(), |_, row| row.0, |_, row| row.0)
        .map_joined_values(project)
        .left_join_by(r5.clone(), |_, row| row.0, |_, row| row.0)
        .map_joined_values(project)
        .left_join_by(r6.clone(), |_, row| row.0, |_, row| row.0)
        .map_joined_values(project)
        .left_join_by(r7.clone(), |_, row| row.0, |_, row| row.0)
        .map_joined_values(project)
        .left_join_by(r8.clone(), |_, row| row.0, |_, row| row.0)
        .map_joined_values(project);
    assert_query(&query);
    let output = query.materialize();

    for (right, key) in [
        (&r1, 1),
        (&r2, 2),
        (&r3, 3),
        (&r4, 4),
        (&r5, 5),
        (&r6, 6),
        (&r7, 7),
        (&r8, 8),
    ] {
        right.insert(key, (key, key + 1, i64::try_from(key).unwrap_or_default()));
    }
    left.insert(99, (1, 0));
    assert_eq!(output.get_value(&99), Some((9, 36)));

    r8.insert(8, (8, 80, 80));
    assert_eq!(output.get_value(&99), Some((80, 108)));
    r1.remove(&1);
    assert_eq!(output.get_value(&99), Some((1, 0)));
}

#[test]
fn rekey_boundary_starts_a_legal_new_join_region() {
    let left = CellMap::<u64, Row>::new();
    let r1 = CellMap::<u64, RightRow>::new();
    let r2 = CellMap::<u64, RightRow>::new();
    let query = left
        .left_join_by(r1, |_, row| row.0, |_, row| row.0)
        .map_joined_values(project)
        .map_entries(|key, row| (key + 100, *row))
        .left_join_by(r2, |_, row| row.0, |_, row| row.0)
        .map_joined_values(project);
    assert_query(&query);
}
