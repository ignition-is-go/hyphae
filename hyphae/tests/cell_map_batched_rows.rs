//! The row diffs a map receives inside one batch reach its diff subscribers
//! as `Batch`es of distinct keys, carrying the rows an unbatched run of the same writes
//! delivers, in the same order. An integration test because `batch` is
//! process-wide.

#![allow(clippy::expect_used, clippy::unreachable)]

use std::{collections::HashMap, sync::Arc, thread};

use hyphae::{CellMap, Gettable, MapDiff, Materialize, batch};
use parking_lot::Mutex;

type Log = Arc<Mutex<Vec<MapDiff<u32, u32>>>>;

static SERIAL: Mutex<()> = Mutex::new(());

fn record(map: &CellMap<u32, u32>) -> (Log, hyphae::SubscriptionGuard) {
    let log: Log = Arc::default();
    let sink = Arc::clone(&log);
    let guard = map.subscribe_diffs(move |diff| sink.lock().push(diff.clone()));
    log.lock().clear();
    (log, guard)
}

fn rows(diffs: &[MapDiff<u32, u32>]) -> Vec<MapDiff<u32, u32>> {
    let mut rows = Vec::new();
    for diff in diffs {
        match diff {
            MapDiff::Batch { changes } => rows.extend(self::rows(changes)),
            row => rows.push(row.clone()),
        }
    }
    rows
}

fn mixed_writes(map: &CellMap<u32, u32>) {
    map.insert(1, 10);
    map.insert(2, 20);
    map.insert(1, 11);
    map.insert(1, 11);
    map.remove(&2);
    map.remove(&9);
    map.insert(2, 21);
    map.insert_many(vec![(3, 30), (1, 12)]);
    map.remove(&1);
    map.apply_diff_owned(MapDiff::Insert { key: 4, value: 40 });
    map.remove_many(vec![3]);
}

#[test]
fn a_batch_delivers_the_unbatched_rows_in_order_without_repeating_a_key() {
    let _serial = SERIAL.lock();
    let unbatched = CellMap::new();
    unbatched.insert(5, 50);
    let (expected, _a) = record(&unbatched);
    mixed_writes(&unbatched);

    let batched = CellMap::new();
    batched.insert(5, 50);
    let (seen, _b) = record(&batched);
    batch(|| mixed_writes(&batched));

    let seen = seen.lock().clone();
    assert_eq!(
        seen.len(),
        4,
        "a delivery per run of distinct keys: {seen:?}"
    );
    for diff in &seen {
        let mut keys: Vec<u32> = rows(std::slice::from_ref(diff))
            .iter()
            .filter_map(|row| match row {
                MapDiff::Insert { key, .. }
                | MapDiff::Remove { key, .. }
                | MapDiff::Update { key, .. } => Some(*key),
                _ => None,
            })
            .collect();
        let len = keys.len();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), len, "a key repeated in one delivery: {diff:?}");
    }
    assert_eq!(rows(&seen), rows(&expected.lock()));
    assert_eq!(
        batched.len().materialize().get(),
        unbatched.len().materialize().get()
    );
}

#[test]
fn writes_outside_a_batch_are_delivered_one_by_one() {
    let _serial = SERIAL.lock();
    let map = CellMap::new();
    let (seen, _guard) = record(&map);
    map.insert(1, 10);
    assert_eq!(*seen.lock(), [MapDiff::Insert { key: 1, value: 10 }]);
    map.insert(1, 11);
    map.remove(&1);
    assert_eq!(seen.lock().len(), 3);
}

#[test]
fn a_single_write_in_a_batch_is_delivered_bare() {
    let _serial = SERIAL.lock();
    let map = CellMap::new();
    let (seen, _guard) = record(&map);
    batch(|| map.insert(1, 10));
    assert_eq!(*seen.lock(), [MapDiff::Insert { key: 1, value: 10 }]);
}

#[test]
fn two_maps_and_nested_batches_each_get_their_own_rows() {
    let _serial = SERIAL.lock();
    let left = CellMap::new();
    let right = CellMap::new();
    let (left_seen, _l) = record(&left);
    let (right_seen, _r) = record(&right);
    batch(|| {
        left.insert(1, 10);
        right.insert(1, 100);
        batch(|| {
            left.insert(2, 20);
            right.remove(&1);
        });
        left.remove(&1);
    });
    assert_eq!(
        *left_seen.lock(),
        [
            MapDiff::Batch {
                changes: vec![
                    MapDiff::Insert { key: 1, value: 10 },
                    MapDiff::Insert { key: 2, value: 20 },
                ]
            },
            MapDiff::Remove {
                key: 1,
                old_value: 10
            },
        ]
    );
    assert_eq!(
        *right_seen.lock(),
        [
            MapDiff::Insert { key: 1, value: 100 },
            MapDiff::Remove {
                key: 1,
                old_value: 100
            },
        ]
    );
}

#[test]
fn point_cells_see_each_key_after_the_batch() {
    let _serial = SERIAL.lock();
    let map = CellMap::new();
    let one = map.get(&1).materialize();
    let two = map.get(&2).materialize();
    batch(|| {
        map.insert(1, 10);
        map.insert(2, 20);
        map.insert(1, 11);
        map.remove(&2);
    });
    assert_eq!(one.get(), Some(11));
    assert_eq!(two.get(), None);
}

/// Replays a diff stream, failing on any row that does not follow from the
/// rows before it.
fn replay(start: &HashMap<u32, u32>, diffs: &[MapDiff<u32, u32>]) -> HashMap<u32, u32> {
    let mut model = start.clone();
    for row in rows(diffs) {
        match row {
            MapDiff::Insert { key, value } => {
                assert_eq!(model.insert(key, value), None, "insert over a live row");
            }
            MapDiff::Update {
                key,
                old_value,
                new_value,
            } => assert_eq!(model.insert(key, new_value), Some(old_value)),
            MapDiff::Remove { key, old_value } => assert_eq!(model.remove(&key), Some(old_value)),
            MapDiff::Initial { entries } => model = entries.into_iter().collect(),
            MapDiff::Batch { .. } => unreachable!("flattened"),
        }
    }
    model
}

#[test]
fn concurrent_batches_on_shared_keys_keep_each_key_in_stored_order() {
    let _serial = SERIAL.lock();
    for _ in 0..50 {
        let map = CellMap::<u32, u32>::new();
        let (seen, _guard) = record(&map);
        let writers: Vec<_> = (0..4u32)
            .map(|writer| {
                let map = map.clone();
                thread::spawn(move || {
                    for round in 0..50u32 {
                        let value = writer * 1000 + round;
                        batch(|| {
                            map.insert(round % 3, value);
                            map.insert(writer + 10, value);
                            map.insert(round % 3, value + 1);
                            if round % 4 == 0 {
                                map.remove(&(round % 3));
                            }
                        });
                    }
                })
            })
            .collect();
        for writer in writers {
            writer.join().expect("writer");
        }
        let final_rows: HashMap<u32, u32> = map.entries().materialize().get().into_iter().collect();
        assert_eq!(replay(&HashMap::new(), &seen.lock()), final_rows);
    }
}
