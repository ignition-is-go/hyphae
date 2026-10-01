use std::sync::{
    Arc, Barrier,
    atomic::{AtomicUsize, Ordering},
};

use super::*;
use crate::traits::{Gettable, Watchable};

const KEY_CELL_CHURN: u64 = 2_000;

#[test]
fn map_diff_traversal_preserves_nested_event_order() {
    let diff = MapDiff::Batch {
        changes: vec![
            MapDiff::Insert {
                key: "a".to_string(),
                value: 1,
            },
            MapDiff::Batch {
                changes: vec![
                    MapDiff::Remove {
                        key: "b".to_string(),
                        old_value: 2,
                    },
                    MapDiff::Initial {
                        entries: vec![("c".to_string(), 3), ("d".to_string(), 4)],
                    },
                ],
            },
        ],
    };

    let mut keys = Vec::new();
    diff.visit_leaves(&mut |change| {
        keys.push(change.atomic_key().cloned());
    });
    assert_eq!(
        keys,
        vec![Some("a".to_string()), Some("b".to_string()), None]
    );
    assert_eq!(diff.work_items(), 4);

    let mut flattened = Vec::new();
    diff.flatten_into(&mut flattened);
    assert!(matches!(
        flattened.as_slice(),
        [
            MapDiff::Insert { .. },
            MapDiff::Remove { .. },
            MapDiff::Initial { .. }
        ]
    ));
}

#[test]
fn test_cellmap_basic() {
    let map = CellMap::<String, i32>::new();

    assert!(map.is_empty());
    assert_eq!(map.get_value(&"a".to_string()), None);

    map.insert("a".to_string(), 1);
    assert_eq!(map.get_value(&"a".to_string()), Some(1));
    assert!(!map.is_empty());

    map.insert("b".to_string(), 2);
    assert_eq!(map.get_value(&"b".to_string()), Some(2));

    let old = map.remove(&"a".to_string());
    assert_eq!(old, Some(1));
    assert_eq!(map.get_value(&"a".to_string()), None);
}

#[test]
fn test_cellmap_per_key_observation() {
    let map = CellMap::<String, i32>::new();

    // Get cell before key exists
    let cell_a = map.get(&"a".to_string()).materialize();
    assert_eq!(cell_a.get(), None);

    let count = Arc::new(AtomicUsize::new(0));
    let c = count.clone();
    let _guard = cell_a.subscribe(move |_| {
        c.fetch_add(1, Ordering::SeqCst);
    });

    assert_eq!(count.load(Ordering::SeqCst), 1); // Initial

    // Insert should trigger update
    map.insert("a".to_string(), 42);
    assert_eq!(cell_a.get(), Some(42));
    assert_eq!(count.load(Ordering::SeqCst), 2);

    // Update should trigger
    map.insert("a".to_string(), 100);
    assert_eq!(cell_a.get(), Some(100));
    assert_eq!(count.load(Ordering::SeqCst), 3);

    // Remove should trigger
    map.remove(&"a".to_string());
    assert_eq!(cell_a.get(), None);
    assert_eq!(count.load(Ordering::SeqCst), 4);
}

#[test]
fn test_cellmap_entries_observation() {
    let map = CellMap::<String, i32>::new();
    let entries = map.entries().materialize();

    assert_eq!(entries.get(), vec![]);

    map.insert("a".to_string(), 1);
    assert_eq!(entries.get().len(), 1);

    map.insert("b".to_string(), 2);
    assert_eq!(entries.get().len(), 2);

    map.remove(&"a".to_string());
    assert_eq!(entries.get().len(), 1);
}

#[test]
fn test_cellmap_size_observation() {
    let map = CellMap::<String, i32>::new();
    let size = map.size().materialize();

    assert_eq!(size.get(), 0);

    map.insert("a".to_string(), 1);
    assert_eq!(size.get(), 1);

    map.insert("b".to_string(), 2);
    assert_eq!(size.get(), 2);

    map.insert("b".to_string(), 2);
    assert_eq!(size.get(), 2);

    map.remove(&"a".to_string());
    assert_eq!(size.get(), 1);
}

#[test]
fn test_cellmap_items_observation() {
    let map = CellMap::<String, i32>::new();
    let items = map.items().materialize();

    assert_eq!(items.get(), Vec::<i32>::new());

    map.insert("a".to_string(), 1);
    assert_eq!(items.get(), vec![1]);

    map.insert("b".to_string(), 2);
    assert_eq!(items.get(), vec![1, 2]);

    map.insert("a".to_string(), 3);
    assert_eq!(items.get(), vec![3, 2]);

    map.remove(&"b".to_string());
    assert_eq!(items.get(), vec![3]);
}

#[test]
fn test_cellmap_diffs() {
    let map = CellMap::<String, i32>::new();
    let diffs = map.diffs().materialize();

    assert_eq!(diffs.get(), MapDiff::Initial { entries: vec![] });

    map.insert("a".to_string(), 1);
    assert_eq!(
        diffs.get(),
        MapDiff::Insert {
            key: "a".to_string(),
            value: 1
        }
    );

    map.insert("a".to_string(), 2);
    assert_eq!(
        diffs.get(),
        MapDiff::Update {
            key: "a".to_string(),
            old_value: 1,
            new_value: 2
        }
    );

    map.remove(&"a".to_string());
    assert_eq!(
        diffs.get(),
        MapDiff::Remove {
            key: "a".to_string(),
            old_value: 2
        }
    );
}

#[test]
fn test_cellmap_subscribe_diffs() {
    let map = CellMap::<String, i32>::new();
    map.insert("a".to_string(), 1);
    map.insert("b".to_string(), 2);

    let (tx, rx) = std::sync::mpsc::channel::<MapDiff<String, i32>>();
    let _guard = map.subscribe_diffs(move |diff| {
        let _ = tx.send(diff.clone());
    });

    // Should have received Initial with both entries
    let diffs: Vec<_> = rx.try_iter().collect();
    assert_eq!(diffs.len(), 1);
    assert!(matches!(
        diffs.first(),
        Some(MapDiff::Initial { entries }) if entries.len() == 2
    ));

    // Insert should trigger diff
    map.insert("c".to_string(), 3);
    let diffs: Vec<_> = rx.try_iter().collect();
    assert_eq!(diffs.len(), 1);
    assert!(
        matches!(diffs.first(), Some(MapDiff::Insert { key, value }) if key == "c" && *value == 3)
    );
}

#[test]
fn test_apply_batch_emits_single_batch_diff() {
    let map = CellMap::<String, i32>::new();
    let (tx, rx) = std::sync::mpsc::channel::<MapDiff<String, i32>>();

    let _guard = map.subscribe_diffs(move |diff| {
        let _ = tx.send(diff.clone());
    });

    map.apply_batch(vec![
        MapDiff::Insert {
            key: "a".to_string(),
            value: 1,
        },
        MapDiff::Insert {
            key: "b".to_string(),
            value: 2,
        },
    ]);

    let seen: Vec<_> = rx.try_iter().collect();
    assert_eq!(seen.len(), 2);
    assert!(matches!(seen.first(), Some(MapDiff::Initial { entries }) if entries.is_empty()));
    assert!(matches!(seen.get(1), Some(MapDiff::Batch { changes }) if changes.len() == 2));
}

#[test]
fn test_insert_same_value_is_noop_update() {
    let map = CellMap::<String, i32>::new();
    let (tx, rx) = std::sync::mpsc::channel::<MapDiff<String, i32>>();

    let _guard = map.subscribe_diffs(move |diff| {
        let _ = tx.send(diff.clone());
    });

    map.insert("a".to_string(), 1);
    map.insert("a".to_string(), 1);

    let seen: Vec<_> = rx.try_iter().collect();
    assert_eq!(seen.len(), 2);
    assert!(matches!(seen.first(), Some(MapDiff::Initial { entries }) if entries.is_empty()));
    assert!(
        matches!(seen.get(1), Some(MapDiff::Insert { key, value }) if key == "a" && *value == 1)
    );
}

#[test]
fn subscribe_diffs_does_not_lose_an_update_during_initial_snapshot() -> Result<(), String> {
    for iteration in 0..10_000 {
        let source = CellMap::<String, usize>::new();
        source.insert("key".to_owned(), 0);
        let replica = CellMap::<String, usize>::new();
        let replica_for_callback = replica.clone();
        let barrier = Arc::new(Barrier::new(2));
        let source_for_thread = source.clone();
        let barrier_for_thread = barrier.clone();
        let subscriber = std::thread::spawn(move || {
            barrier_for_thread.wait();
            source_for_thread.subscribe_diffs(move |diff| {
                replica_for_callback.apply_diff_owned(diff.clone());
            })
        });

        barrier.wait();
        source.insert("key".to_owned(), 1);
        let guard = subscriber
            .join()
            .map_err(|_| format!("subscriber thread panicked at iteration {iteration}"))?;
        if replica.get_value(&"key".to_owned()) != Some(1) {
            return Err(format!(
                "subscriber stranded the initial value at iteration {iteration}"
            ));
        }
        drop(guard);
    }
    Ok(())
}

#[test]
fn subscribe_diffs_concurrent_insert_and_remove_converge() -> Result<(), String> {
    for iteration in 0..10_000 {
        let source = CellMap::<String, usize>::new();
        if iteration % 2 == 0 {
            source.insert("key".to_owned(), 0);
        }
        let replica = CellMap::<String, usize>::new();
        let replica_for_callback = replica.clone();
        let barrier = Arc::new(Barrier::new(2));
        let source_for_thread = source.clone();
        let barrier_for_thread = barrier.clone();
        let subscriber = std::thread::spawn(move || {
            barrier_for_thread.wait();
            source_for_thread.subscribe_diffs(move |diff| {
                replica_for_callback.apply_diff_owned(diff.clone());
            })
        });

        barrier.wait();
        let expected = if iteration % 2 == 0 {
            source.remove(&"key".to_owned());
            None
        } else {
            source.insert("key".to_owned(), 1);
            Some(1)
        };
        let guard = subscriber
            .join()
            .map_err(|_| format!("subscriber thread panicked at iteration {iteration}"))?;
        if replica.get_value(&"key".to_owned()) != expected {
            return Err(format!(
                "subscriber did not converge after insert/remove at iteration {iteration}"
            ));
        }
        drop(guard);
    }
    Ok(())
}

#[test]
fn subscribe_diffs_reentrant_update_follows_initial_snapshot() {
    let source = CellMap::<String, usize>::new();
    source.insert("key".to_owned(), 0);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let seen_for_callback = seen.clone();
    let source_for_callback = source.clone();

    let _guard = source.subscribe_diffs(move |diff| {
        seen_for_callback.lock().push(diff.clone());
        if matches!(diff, MapDiff::Initial { .. }) {
            source_for_callback.insert("key".to_owned(), 1);
        }
    });

    {
        let seen = seen.lock();
        assert!(
            matches!(seen.first(), Some(MapDiff::Initial { entries }) if entries == &[("key".to_owned(), 0)])
        );
        assert!(
            matches!(seen.get(1), Some(MapDiff::Update { key, old_value: 0, new_value: 1 }) if key == "key")
        );
        drop(seen);
    }
}

#[test]
fn test_apply_batch_filters_noop_updates() {
    let map = CellMap::<String, i32>::new();
    let (tx, rx) = std::sync::mpsc::channel::<MapDiff<String, i32>>();

    map.insert("a".to_string(), 1);
    let _guard = map.subscribe_diffs(move |diff| {
        let _ = tx.send(diff.clone());
    });

    map.apply_batch(vec![
        MapDiff::Update {
            key: "a".to_string(),
            old_value: 1,
            new_value: 1,
        },
        MapDiff::Update {
            key: "a".to_string(),
            old_value: 1,
            new_value: 2,
        },
    ]);

    let seen: Vec<_> = rx.try_iter().collect();
    assert_eq!(seen.len(), 2);
    assert!(matches!(seen.first(), Some(MapDiff::Initial { entries }) if entries.len() == 1));
    assert!(matches!(
        seen.get(1),
        Some(MapDiff::Batch { changes })
            if matches!(changes.as_slice(), [MapDiff::Update { key, old_value: 1, new_value: 2 }] if key == "a")
    ));

    assert_eq!(map.get_value(&"a".to_string()), Some(2));
}

#[test]
fn test_remove_many_full_clear_emits_empty_initial() {
    let map = CellMap::<String, i32>::new();
    let (tx, rx) = std::sync::mpsc::channel::<MapDiff<String, i32>>();

    map.insert("a".to_string(), 1);
    map.insert("b".to_string(), 2);

    let _guard = map.subscribe_diffs(move |diff| {
        let _ = tx.send(diff.clone());
    });

    map.remove_many(vec!["a".to_string(), "b".to_string()]);

    let seen: Vec<_> = rx.try_iter().collect();
    assert_eq!(seen.len(), 2);
    assert!(matches!(seen.first(), Some(MapDiff::Initial { entries }) if entries.len() == 2));
    assert!(matches!(seen.get(1), Some(MapDiff::Initial { entries }) if entries.is_empty()));
}

#[test]
fn test_cellmap_len() {
    let map = CellMap::<String, i32>::new();
    let len = map.len().materialize();

    assert_eq!(len.get(), 0);

    map.insert("a".to_string(), 1);
    assert_eq!(len.get(), 1);

    map.insert("b".to_string(), 2);
    assert_eq!(len.get(), 2);

    map.remove(&"a".to_string());
    assert_eq!(len.get(), 1);
}

#[test]
fn test_len_notifies_only_for_cardinality_changes_across_mutation_paths() {
    let map = CellMap::<String, i32>::new();
    let len = map.len().materialize();
    let notifications = Arc::new(AtomicUsize::new(0));
    let observed = notifications.clone();
    let _guard = len.subscribe(move |_| {
        observed.fetch_add(1, Ordering::SeqCst);
    });
    assert_eq!(notifications.load(Ordering::SeqCst), 1);

    map.insert("a".into(), 1);
    assert_eq!(notifications.load(Ordering::SeqCst), 2);
    map.insert("a".into(), 2);
    assert_eq!(notifications.load(Ordering::SeqCst), 2);

    map.insert_many(vec![("a".into(), 3), ("b".into(), 4)]);
    assert_eq!(notifications.load(Ordering::SeqCst), 3);
    map.insert_many(vec![("a".into(), 5), ("b".into(), 6)]);
    assert_eq!(notifications.load(Ordering::SeqCst), 3);

    // Replacing keys and values at the same cardinality must not wake size.
    map.replace_all(vec![("a".into(), 7), ("c".into(), 8)]);
    assert_eq!(notifications.load(Ordering::SeqCst), 3);

    map.apply_diff_owned(MapDiff::Update {
        key: "a".into(),
        old_value: 7,
        new_value: 9,
    });
    assert_eq!(notifications.load(Ordering::SeqCst), 3);
    map.apply_batch(vec![
        MapDiff::Remove {
            key: "c".into(),
            old_value: 8,
        },
        MapDiff::Insert {
            key: "d".into(),
            value: 10,
        },
    ]);
    assert_eq!(notifications.load(Ordering::SeqCst), 3);
    map.apply_diff_owned(MapDiff::Initial {
        entries: vec![("x".into(), 1), ("y".into(), 2)],
    });
    assert_eq!(notifications.load(Ordering::SeqCst), 3);

    map.remove_many(vec!["x".into()]);
    assert_eq!(len.get(), 1);
    assert_eq!(notifications.load(Ordering::SeqCst), 4);
    map.replace_all(vec![]);
    assert_eq!(notifications.load(Ordering::SeqCst), 5);
}

#[test]
fn test_keys_projection_ignores_value_updates() {
    let map = CellMap::<String, i32>::new();
    map.insert("a".into(), 1);
    let keys = map.keys().materialize();
    let notifications = Arc::new(AtomicUsize::new(0));
    let observed = notifications.clone();
    let _guard = keys.subscribe(move |_| {
        observed.fetch_add(1, Ordering::SeqCst);
    });
    assert_eq!(notifications.load(Ordering::SeqCst), 1);

    map.insert("a".into(), 2);
    map.apply_diff_owned(MapDiff::Update {
        key: "a".into(),
        old_value: 2,
        new_value: 3,
    });
    map.apply_batch(vec![MapDiff::Update {
        key: "a".into(),
        old_value: 3,
        new_value: 4,
    }]);
    assert_eq!(notifications.load(Ordering::SeqCst), 1);
    assert_eq!(keys.get(), vec!["a".to_string()]);

    map.insert("b".into(), 5);
    assert_eq!(notifications.load(Ordering::SeqCst), 2);
    let mut projected = keys.get();
    projected.sort();
    assert_eq!(projected, vec!["a".to_string(), "b".to_string()]);
}

#[test]
fn projection_owner_recovers_poisoned_state() {
    let owner = ProjectionOwner::new(KeyProjection::from_keys(vec!["a".to_string()]));
    let poisoned = owner.clone();
    let panic = std::thread::spawn(move || {
        let _state = poisoned
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::panic::resume_unwind(Box::new("poison projection state"));
    })
    .join();
    assert!(panic.is_err());

    let output = owner.with(|projection| {
        projection.apply_diff(&MapDiff::<String, i32>::Insert {
            key: "b".to_string(),
            value: 2,
        });
        projection.keys()
    });
    assert_eq!(output, vec!["a".to_string(), "b".to_string()]);
}

#[test]
fn test_key_and_item_snapshots_are_one_shot_views() {
    let map = CellMap::<String, i32>::new();
    map.insert_many(vec![("a".into(), 1), ("b".into(), 2)]);

    let mut keys = map.keys_snapshot();
    keys.sort();
    let mut items = map.items_snapshot();
    items.sort_unstable();
    assert_eq!(keys, vec!["a".to_string(), "b".to_string()]);
    assert_eq!(items, vec![1, 2]);

    map.insert("c".into(), 3);
    assert_eq!(keys.len(), 2);
    assert_eq!(items.len(), 2);
}

#[test]
fn test_cellmap_lock() {
    let map = CellMap::<String, i32>::new();
    map.insert("a".to_string(), 1);

    let locked = map.lock();

    // Can still observe
    assert_eq!(locked.get(&"a".to_string()).materialize().get(), Some(1));
    assert_eq!(locked.entries().materialize().get().len(), 1);

    // But can't mutate - these methods don't exist on CellImmutable
    // locked.insert(...) // compile error
}

#[test]
fn test_cellmap_same_cell_returned() {
    let map = CellMap::<String, i32>::new();

    let cell1 = map.get(&"a".to_string()).materialize();
    let cell2 = map.get(&"a".to_string()).materialize();

    // Both should reflect same updates
    map.insert("a".to_string(), 42);

    assert_eq!(cell1.get(), Some(42));
    assert_eq!(cell2.get(), Some(42));
}

#[test]
fn concurrent_gets_share_the_single_notified_cell() {
    const READERS: usize = 32;
    let map = CellMap::<String, i32>::new();
    let barrier = Arc::new(Barrier::new(READERS));
    let (tx, rx) = std::sync::mpsc::channel();

    for _ in 0..READERS {
        let map = map.clone();
        let barrier = Arc::clone(&barrier);
        let tx = tx.clone();
        drop(std::thread::spawn(move || {
            barrier.wait();
            let _ = tx.send(map.get(&"key".to_string()).materialize());
        }));
    }
    drop(tx);

    let cells: Vec<_> = rx.into_iter().collect();
    assert_eq!(cells.len(), READERS);
    assert_eq!(map.inner.key_cell_slots.load(Ordering::Relaxed), 1);
    assert_eq!(map.inner.key_cells.len(), 1);
    map.insert("key".to_string(), 42);

    assert!(cells.iter().all(|cell| cell.get() == Some(42)));
}

#[test]
fn test_cellmap_mutable_clone_shares_inner_map() {
    let map = CellMap::<String, i32>::new();
    let map_clone = map.clone();

    map.insert("a".to_string(), 1);
    assert_eq!(map_clone.get_value(&"a".to_string()), Some(1));

    map_clone.insert("b".to_string(), 2);
    assert_eq!(map.get_value(&"b".to_string()), Some(2));
    assert_eq!(map.len().materialize().get(), 2);
    assert_eq!(map_clone.len().materialize().get(), 2);
}

#[test]
fn test_replace_all() {
    let map = CellMap::<String, i32>::new();
    map.insert("a".to_string(), 1);
    map.insert("b".to_string(), 2);
    map.insert("c".to_string(), 3);

    let (tx, rx) = std::sync::mpsc::channel::<MapDiff<String, i32>>();
    let _guard = map.subscribe_diffs(move |diff| {
        let _ = tx.send(diff.clone());
    });

    // Replace: keep "a" (updated), add "d", remove "b" and "c"
    map.replace_all(vec![("a".to_string(), 10), ("d".to_string(), 4)]);

    assert_eq!(map.len().materialize().get(), 2);
    assert_eq!(map.get_value(&"a".to_string()), Some(10));
    assert_eq!(map.get_value(&"d".to_string()), Some(4));
    assert_eq!(map.get_value(&"b".to_string()), None);
    assert_eq!(map.get_value(&"c".to_string()), None);

    // Initial snapshot + replace_all Batch
    let seen: Vec<_> = rx.try_iter().collect();
    assert_eq!(seen.len(), 2);
    assert!(matches!(seen.get(1), Some(MapDiff::Batch { changes }) if changes.len() == 4));
}

#[test]
fn test_replace_all_noop() {
    let map = CellMap::<String, i32>::new();
    map.insert("a".to_string(), 1);

    // Replace with same data
    map.replace_all(vec![("a".to_string(), 1)]);

    assert_eq!(map.len().materialize().get(), 1);
    assert_eq!(map.get_value(&"a".to_string()), Some(1));
}

#[test]
fn test_replace_all_empty() {
    let map = CellMap::<String, i32>::new();
    map.insert("a".to_string(), 1);
    map.insert("b".to_string(), 2);

    map.replace_all(vec![]);

    assert_eq!(map.len().materialize().get(), 0);
    assert_eq!(map.get_value(&"a".to_string()), None);
    assert_eq!(map.get_value(&"b".to_string()), None);
}

// Regression: `key_cells` must not grow without bound as distinct keys are
// observed via `get` and then their cells are dropped. Before the amortized
// sweep, every distinct key ever passed to `get` left a dangling `WeakCell`
// slot behind forever — the production OOM in rship.
#[test]
fn test_key_cells_pruned_under_churn() {
    let map = CellMap::<u64, u64>::new();

    // Churn a large number of distinct keys: observe each (creating a
    // key_cells slot), drop the observer, then drive mutations.
    for i in 0..KEY_CELL_CHURN {
        let cell = map.get(&i).materialize();
        assert_eq!(cell.get(), None);
        drop(cell); // observer gone → this key's weak now dangles
        map.insert(i, i);
        map.remove(&i);
    }

    // Amortized pruning must have kept key_cells bounded far below the
    // number of distinct keys churned (it would otherwise be ~CHURN).
    let live = map.inner.key_cells.len();
    assert!(
        live < 256,
        "key_cells should stay bounded under churn, got {live} slots after {KEY_CELL_CHURN} keys",
    );
}

// Pruning must never evict a still-observed key: a live cell held across a
// churn storm must keep receiving updates (reinsert re-notify preserved).
#[test]
fn test_key_cells_prune_keeps_live_observer() {
    let map = CellMap::<u64, u64>::new();

    // A long-lived observer on a stable key.
    let watched = map.get(&0).materialize();
    let hits = Arc::new(AtomicUsize::new(0));
    let h = hits.clone();
    let _guard = watched.subscribe(move |_| {
        h.fetch_add(1, Ordering::SeqCst);
    });
    let initial = hits.load(Ordering::SeqCst);

    // Churn many other keys to trigger repeated sweeps.
    for i in 1..2_000u64 {
        let cell = map.get(&i).materialize();
        drop(cell);
        map.insert(i, i);
        map.remove(&i);
    }

    // The watched key's live cell survived every sweep and still updates.
    map.insert(0, 99);
    assert_eq!(watched.get(), Some(99));
    assert!(
        hits.load(Ordering::SeqCst) > initial,
        "live observer must still be notified after pruning sweeps",
    );
}

#[test]
fn existing_key_diff_does_not_wait_for_unrelated_data_shard() {
    for overwrite in [false, true] {
        let map = CellMap::<u64, u64>::new();
        for key in 0..256 {
            map.insert(key, key);
        }
        let Some(held) = map.inner.data.get_mut(&0) else {
            std::panic::resume_unwind(Box::new("missing shard-lock fixture key"));
        };
        let target = (1..256).find(|key| {
            matches!(
                map.inner.data.try_get(key),
                dashmap::try_result::TryResult::Present(_)
            )
        });
        let target_key = target.unwrap_or(1);
        let updater = map.clone();
        let (completed, completion) = std::sync::mpsc::channel();
        let (started, start) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let _ = started.send(());
            let diff = if overwrite {
                MapDiff::Insert {
                    key: target_key,
                    value: 999,
                }
            } else {
                MapDiff::Update {
                    key: target_key,
                    old_value: target_key,
                    new_value: 999,
                }
            };
            updater.apply_diff_owned(diff);
            let _ = completed.send(());
        });
        let worker_started = start
            .recv_timeout(std::time::Duration::from_secs(5))
            .is_ok();
        let finished_while_held = completion
            .recv_timeout(std::time::Duration::from_secs(5))
            .is_ok();
        drop(held);
        let joined = worker.join().is_ok();
        assert!(target.is_some(), "fixture must use separate data shards");
        assert!(worker_started);
        assert!(joined);
        assert!(
            finished_while_held,
            "existing-key diffs must not scan unrelated data shards"
        );
        assert_eq!(map.get_value(&target_key), Some(999));
    }
}

#[test]
fn existing_key_update_does_not_scan_unrelated_key_cache_shard() {
    let map = CellMap::<u64, u64>::new();
    for key in 0..256 {
        map.insert(key, key);
    }
    let observers: Vec<_> = (0..256).map(|key| map.get(&key).materialize()).collect();
    let Some(held) = map.inner.key_cells.get_mut(&0) else {
        std::panic::resume_unwind(Box::new("missing key-cache fixture slot"));
    };
    let target = (1..256).find(|key| {
        matches!(
            map.inner.key_cells.try_get(key),
            dashmap::try_result::TryResult::Present(_)
        )
    });
    let target_key = target.unwrap_or(1);
    let updater = map.clone();
    let (completed, completion) = std::sync::mpsc::channel();
    let (started, start) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let _ = started.send(());
        updater.apply_diff_owned(MapDiff::Update {
            key: target_key,
            old_value: target_key,
            new_value: 999,
        });
        let _ = completed.send(());
    });
    let worker_started = start
        .recv_timeout(std::time::Duration::from_secs(5))
        .is_ok();
    let finished_while_held = completion
        .recv_timeout(std::time::Duration::from_secs(5))
        .is_ok();
    drop(held);
    let joined = worker.join().is_ok();
    assert!(
        target.is_some(),
        "fixture must use separate key-cache shards"
    );
    assert!(worker_started);
    assert!(joined);
    assert!(
        finished_while_held,
        "prune gate must not scan unrelated key-cache shards"
    );
    assert_eq!(map.get_value(&target_key), Some(999));
    assert_eq!(
        observers
            .iter()
            .filter(|observer| observer.get() == Some(999))
            .count(),
        1
    );
}

#[test]
fn key_cell_slot_count_tracks_dead_replacement_and_pruning() {
    let map = CellMap::<u64, u64>::new();
    drop(map.get(&0).materialize());
    assert_eq!(map.inner.key_cell_slots.load(Ordering::Relaxed), 1);
    let watched = map.get(&0).materialize();
    assert_eq!(map.inner.key_cell_slots.load(Ordering::Relaxed), 1);
    for key in 1..65 {
        drop(map.get(&key).materialize());
    }
    assert_eq!(map.inner.key_cell_slots.load(Ordering::Relaxed), 65);
    for value in 0..130 {
        map.insert(999, value);
    }
    assert_eq!(map.inner.key_cells.len(), 1);
    assert_eq!(map.inner.key_cell_slots.load(Ordering::Relaxed), 1);
    map.insert(0, 42);
    assert_eq!(watched.get(), Some(42));
    drop(watched);
    for value in 0..32 {
        map.insert(999, value);
    }
    assert_eq!(map.inner.key_cells.len(), 0);
    assert_eq!(map.inner.key_cell_slots.load(Ordering::Relaxed), 0);
}

#[test]
fn concurrent_key_observation_and_pruning_keep_exact_quiescent_slot_count() {
    let map = CellMap::<String, u64>::new();
    let watched = map.get(&"live".to_owned()).materialize();
    let barrier = Arc::new(Barrier::new(4));
    std::thread::scope(|scope| {
        for reader in 0..4 {
            let map = map.clone();
            let barrier = barrier.clone();
            scope.spawn(move || {
                barrier.wait();
                for step in 0..200 {
                    drop(map.get(&format!("{reader}:{step}")).materialize());
                    map.insert("updates".to_owned(), step);
                }
            });
        }
    });
    assert_eq!(
        map.inner.key_cell_slots.load(Ordering::Relaxed),
        map.inner.key_cells.len()
    );
    for value in 0..1_000 {
        map.insert("updates".to_owned(), value);
    }
    assert_eq!(map.inner.key_cell_slots.load(Ordering::Relaxed), 1);
    assert_eq!(map.inner.key_cells.len(), 1);
    map.insert("live".to_owned(), 42);
    assert_eq!(watched.get(), Some(42));
}

#[test]
fn single_diff_upserts_and_overwrites_preserve_length_and_notifications() {
    let map = CellMap::<String, i32>::new();
    let length = map.len().materialize();
    let key = map.get(&"key".to_owned()).materialize();
    let lengths = Arc::new(Mutex::new(Vec::new()));
    let observed_lengths = lengths.clone();
    let _length_guard = length.subscribe(move |signal| {
        if let Signal::Value(value) = signal {
            observed_lengths.lock().push(**value);
        }
    });
    let key_values = Arc::new(Mutex::new(Vec::new()));
    let observed_values = key_values.clone();
    let _key_guard = key.subscribe(move |signal| {
        if let Signal::Value(value) = signal {
            observed_values.lock().push(**value);
        }
    });
    let diffs = Arc::new(Mutex::new(Vec::new()));
    let observed_diffs = diffs.clone();
    let _diff_guard = map.subscribe_diffs(move |diff| observed_diffs.lock().push(diff.clone()));
    let changes = vec![
        MapDiff::Update {
            key: "key".to_owned(),
            old_value: 100,
            new_value: 5,
        },
        MapDiff::Insert {
            key: "key".to_owned(),
            value: 6,
        },
        MapDiff::Remove {
            key: "absent".to_owned(),
            old_value: 9,
        },
        MapDiff::Remove {
            key: "key".to_owned(),
            old_value: 6,
        },
    ];
    for diff in &changes {
        map.apply_diff_owned(diff.clone());
        if let MapDiff::Update { .. } = diff {
            map.apply_diff_owned(MapDiff::Update {
                key: "key".to_owned(),
                old_value: 0,
                new_value: 5,
            });
        }
    }
    assert_eq!(*lengths.lock(), vec![0, 1, 0]);
    assert_eq!(*key_values.lock(), vec![None, Some(5), Some(6), None]);
    let mut expected = vec![MapDiff::Initial {
        entries: Vec::new(),
    }];
    expected.extend(changes);
    assert_eq!(*diffs.lock(), expected);
}

#[test]
fn existing_key_diff_preserves_reentrant_membership_changes() {
    for overwrite in [false, true] {
        let map = CellMap::<String, i32>::new();
        map.insert("watched".to_owned(), 1);
        map.insert("removable".to_owned(), 7);
        let length = map.len().materialize();
        let lengths = Arc::new(Mutex::new(Vec::new()));
        let observed_lengths = lengths.clone();
        let _length_guard = length.subscribe(move |signal| {
            if let Signal::Value(value) = signal {
                observed_lengths.lock().push(**value);
            }
        });
        let diffs = Arc::new(Mutex::new(Vec::new()));
        let observed_diffs = diffs.clone();
        let _diff_guard = map.subscribe_diffs(move |diff| {
            observed_diffs.lock().push(diff.clone());
        });
        let watched = map.get(&"watched".to_owned()).materialize();
        let reacting = map.clone();
        let _key_guard = watched.subscribe(move |signal| {
            if let Signal::Value(value) = signal
                && value.as_ref() == &Some(2)
            {
                reacting.remove(&"removable".to_owned());
                reacting.insert("added".to_owned(), 9);
            }
        });
        let diff = if overwrite {
            MapDiff::Insert {
                key: "watched".to_owned(),
                value: 2,
            }
        } else {
            MapDiff::Update {
                key: "watched".to_owned(),
                old_value: 1,
                new_value: 2,
            }
        };
        map.apply_diff_owned(diff.clone());
        assert_eq!(length.get(), 2);
        assert_eq!(map.get_value(&"removable".to_owned()), None);
        assert_eq!(map.get_value(&"added".to_owned()), Some(9));
        assert_eq!(*lengths.lock(), vec![2, 1, 2]);
        assert_eq!(
            diffs.lock().iter().skip(1).cloned().collect::<Vec<_>>(),
            vec![
                MapDiff::Remove {
                    key: "removable".to_owned(),
                    old_value: 7
                },
                MapDiff::Insert {
                    key: "added".to_owned(),
                    value: 9
                },
                diff,
            ],
        );
    }
}
