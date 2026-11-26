//! Every write a batch makes to one key reaches diff subscribers as its own
//! emission, in write order, even where writes to distinct keys share one
//! `Batch`. An integration test because `batch` is process-wide.

#![allow(clippy::expect_used, clippy::panic)]

use hyphae::{CellMap, LeftJoinExt, MapDiff, MapQuery, SubscriptionGuard, batch};
use parking_lot::Mutex;
use std::sync::Arc;

type Log = Arc<Mutex<Vec<MapDiff<u32, u32>>>>;

static SERIAL: Mutex<()> = Mutex::new(());

fn record<M>(map: &CellMap<u32, u32, M>) -> (Log, SubscriptionGuard) {
    let log: Log = Arc::default();
    let sink = Arc::clone(&log);
    let guard = map.subscribe_diffs(move |diff| sink.lock().push(diff.clone()));
    log.lock().clear();
    (log, guard)
}

const fn insert(key: u32, value: u32) -> MapDiff<u32, u32> {
    MapDiff::Insert { key, value }
}

const fn update(key: u32, old_value: u32, new_value: u32) -> MapDiff<u32, u32> {
    MapDiff::Update {
        key,
        old_value,
        new_value,
    }
}

const fn rows(changes: Vec<MapDiff<u32, u32>>) -> MapDiff<u32, u32> {
    MapDiff::Batch { changes }
}

#[test]
fn three_writes_to_one_key_are_three_emissions_in_order() {
    let _serial = SERIAL.lock();
    let map = CellMap::new();
    let (seen, _guard) = record(&map);
    batch(|| {
        map.insert(1, 10);
        map.insert(1, 11);
        map.insert(1, 12);
    });
    assert_eq!(
        *seen.lock(),
        [insert(1, 10), update(1, 10, 11), update(1, 11, 12)]
    );
}

#[test]
fn writes_to_distinct_keys_are_one_emission() {
    let _serial = SERIAL.lock();
    let map = CellMap::new();
    let (seen, _guard) = record(&map);
    batch(|| (0..4).for_each(|key| _ = map.insert(key, key)));
    assert_eq!(
        *seen.lock(),
        [rows(vec![
            insert(0, 0),
            insert(1, 1),
            insert(2, 2),
            insert(3, 3)
        ])]
    );
}

#[test]
fn a_repeated_key_starts_the_next_emission() {
    let _serial = SERIAL.lock();
    let map = CellMap::new();
    let (seen, _guard) = record(&map);
    batch(|| {
        map.insert(1, 10);
        map.insert(2, 20);
        map.insert(1, 11);
        map.insert(3, 30);
        map.insert(2, 21);
    });
    assert_eq!(
        *seen.lock(),
        [
            rows(vec![insert(1, 10), insert(2, 20)]),
            rows(vec![update(1, 10, 11), insert(3, 30), update(2, 20, 21)]),
        ]
    );
}

#[test]
fn a_repeated_key_across_nested_batches_is_its_own_emission() {
    let _serial = SERIAL.lock();
    let map = CellMap::new();
    let (seen, _guard) = record(&map);
    batch(|| {
        map.insert(1, 10);
        batch(|| {
            map.insert(2, 20);
            map.insert(1, 11);
        });
        map.insert(1, 12);
    });
    assert_eq!(
        *seen.lock(),
        [
            rows(vec![insert(1, 10), insert(2, 20)]),
            update(1, 10, 11),
            update(1, 11, 12),
        ]
    );
}

#[test]
fn a_callers_batch_keeps_its_own_rows() {
    let _serial = SERIAL.lock();
    let map = CellMap::new();
    let (seen, _guard) = record(&map);
    let own = rows(vec![insert(1, 10), update(1, 10, 11)]);
    batch(|| {
        map.insert(2, 20);
        map.apply_diff_owned(own.clone());
        map.insert(1, 12);
    });
    assert_eq!(
        *seen.lock(),
        [
            rows(vec![insert(2, 20), insert(1, 10), update(1, 10, 11)]),
            update(1, 11, 12),
        ]
    );
}

#[test]
fn a_join_downstream_sees_every_write_to_one_key() {
    let _serial = SERIAL.lock();
    let left = CellMap::<u32, u32>::new();
    let right = CellMap::<u32, u32>::new();
    right.insert(0, 0);
    let joined = left
        .clone()
        .left_join_by(right, |_, _| 0, |_, _| 0)
        .map_joined_values(|_, value, _| *value)
        .materialize();
    let (seen, _guard) = record(&joined);
    batch(|| {
        left.insert(1, 10);
        left.insert(1, 11);
        left.insert(1, 12);
    });
    let mut values = Vec::new();
    for diff in seen.lock().iter() {
        match diff {
            MapDiff::Batch { changes } => values.extend(changes.iter().map(value)),
            row => values.push(value(row)),
        }
    }
    assert_eq!(values, [10, 11, 12]);
}

fn value(diff: &MapDiff<u32, u32>) -> u32 {
    match diff {
        MapDiff::Insert { value, .. }
        | MapDiff::Update {
            new_value: value, ..
        } => *value,
        other => panic!("unexpected {other:?}"),
    }
}
