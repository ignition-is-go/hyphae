use std::{
    collections::HashMap,
    hash::Hash,
    sync::{
        Arc, Barrier,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    time::Duration,
};

use parking_lot::Mutex;

use super::{CellMap, MapDiff, MapWrite};
use crate::{subscription::SubscriptionGuard, traits::CellValue};

/// A secondary "index" that mirrors the map exactly. Every event is checked
/// against the mirror's prior state, so a missed, doubled, or reordered call
/// fails at the event that exposes it.
struct Shadow<K, V> {
    rows: Arc<Mutex<HashMap<K, V>>>,
    calls: Arc<AtomicUsize>,
    _guard: SubscriptionGuard,
}

impl<K, V> Shadow<K, V>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    fn attach(map: &CellMap<K, V>) -> Self {
        let rows = Arc::new(Mutex::new(HashMap::new()));
        let calls = Arc::new(AtomicUsize::new(0));
        let (hook_rows, hook_calls) = (rows.clone(), calls.clone());
        let guard = map.register_write_hook(move |write| {
            hook_calls.fetch_add(1, Ordering::Relaxed);
            let mut rows = hook_rows.lock();
            match write {
                MapWrite::Insert { key, value } => {
                    assert!(
                        rows.insert(key.clone(), value.clone()).is_none(),
                        "insert of a present key {key:?}"
                    );
                }
                MapWrite::Update { key, old, new } => {
                    assert_ne!(old, new, "equal update reported");
                    let prior = rows.insert(key.clone(), new.clone());
                    assert_eq!(
                        prior.as_ref(),
                        Some(old),
                        "update old value mismatch for {key:?}"
                    );
                }
                MapWrite::Remove { key, old } => {
                    assert_eq!(
                        rows.remove(key).as_ref(),
                        Some(old),
                        "remove old value mismatch for {key:?}"
                    );
                }
            }
        });
        Self {
            rows,
            calls,
            _guard: guard,
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::Relaxed)
    }

    #[track_caller]
    fn assert_matches(&self, map: &CellMap<K, V>) {
        let expected: HashMap<K, V> = map.snapshot().into_iter().collect();
        assert_eq!(*self.rows.lock(), expected);
    }
}

fn seeded() -> (CellMap<u32, u32>, Shadow<u32, u32>) {
    let map = CellMap::new();
    for key in 0..8 {
        map.insert(key, key);
    }
    let shadow = Shadow::attach(&map);
    shadow.assert_matches(&map);
    (map, shadow)
}

#[test]
fn insert_reports_new_and_changed_keys_only() {
    let (map, shadow) = seeded();
    let base = shadow.calls();
    map.insert(100, 1);
    map.insert(100, 2);
    map.insert(100, 2);
    assert_eq!(
        shadow.calls() - base,
        2,
        "the equal insert must not call the hook"
    );
    shadow.assert_matches(&map);
}

#[test]
fn remove_reports_present_keys_only() {
    let (map, shadow) = seeded();
    let base = shadow.calls();
    assert_eq!(map.remove(&3), Some(3));
    assert_eq!(map.remove(&3), None);
    assert_eq!(shadow.calls() - base, 1);
    shadow.assert_matches(&map);
}

#[test]
fn insert_many_reports_each_changed_row() {
    let (map, shadow) = seeded();
    let base = shadow.calls();
    map.insert_many(vec![(1, 1), (2, 20), (50, 50)]);
    assert_eq!(shadow.calls() - base, 2);
    shadow.assert_matches(&map);
}

#[test]
fn remove_many_reports_each_removed_row() {
    let (map, shadow) = seeded();
    map.remove_many(vec![1, 2, 99]);
    shadow.assert_matches(&map);
    map.remove_many(map.keys_snapshot());
    assert!(map.is_empty());
    shadow.assert_matches(&map);
}

#[test]
fn replace_all_reports_removes_inserts_and_updates() {
    let (map, shadow) = seeded();
    let base = shadow.calls();
    map.replace_all(vec![(0, 0), (1, 10), (77, 7)]);
    // 6 removed, 1 updated, 1 inserted, key 0 unchanged.
    assert_eq!(shadow.calls() - base, 8);
    shadow.assert_matches(&map);
}

#[test]
fn apply_diff_owned_reports_every_variant() {
    let (map, shadow) = seeded();
    map.apply_diff_owned(MapDiff::Insert { key: 40, value: 4 });
    shadow.assert_matches(&map);
    map.apply_diff_owned(MapDiff::Insert { key: 40, value: 5 });
    shadow.assert_matches(&map);
    map.apply_diff_owned(MapDiff::Update {
        key: 40,
        old_value: 5,
        new_value: 6,
    });
    shadow.assert_matches(&map);
    map.apply_diff_owned(MapDiff::Remove {
        key: 40,
        old_value: 6,
    });
    shadow.assert_matches(&map);
    map.apply_diff_owned(MapDiff::Remove {
        key: 40,
        old_value: 6,
    });
    shadow.assert_matches(&map);
    map.apply_diff_owned(MapDiff::Batch {
        changes: vec![MapDiff::Insert { key: 41, value: 1 }],
    });
    shadow.assert_matches(&map);
    map.apply_diff_owned(MapDiff::Initial {
        entries: vec![(1, 1), (2, 200)],
    });
    shadow.assert_matches(&map);
}

#[test]
fn apply_batch_reports_nested_and_initial_changes() {
    let (map, shadow) = seeded();
    map.apply_batch(vec![
        MapDiff::Insert { key: 60, value: 1 },
        MapDiff::Remove {
            key: 0,
            old_value: 0,
        },
        MapDiff::Update {
            key: 1,
            old_value: 1,
            new_value: 11,
        },
        MapDiff::Batch {
            changes: vec![MapDiff::Insert { key: 61, value: 2 }],
        },
    ]);
    shadow.assert_matches(&map);
    map.apply_batch(vec![MapDiff::Initial {
        entries: vec![(9, 9)],
    }]);
    shadow.assert_matches(&map);
}

#[test]
fn several_hooks_each_see_every_write_and_dropping_one_stops_it() {
    let map = CellMap::<u32, u32>::new();
    let first = Shadow::attach(&map);
    let second = Shadow::attach(&map);
    map.insert(1, 1);
    first.assert_matches(&map);
    second.assert_matches(&map);
    let calls = second.calls();
    drop(second);
    map.insert(2, 2);
    first.assert_matches(&map);
    drop(first);
    map.insert(3, 3);
    assert_eq!(calls, 1);
}

#[test]
fn hook_dropped_after_its_map_is_harmless() {
    let map = CellMap::<u32, u32>::new();
    let shadow = Shadow::attach(&map);
    drop(map);
    drop(shadow);
}

/// The source scan behind every per-path test above: map data may only be
/// mutated by the two helpers that report to hooks, and the public mutating
/// API is a closed list each member of which has a test in this file.
#[test]
fn every_data_mutation_goes_through_the_hooked_helpers() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mutators = [
        ".insert(",
        ".remove(",
        ".remove_if(",
        ".clear(",
        ".retain(",
        ".alter(",
        ".get_mut(",
        ".entry(",
        ".iter_mut(",
        ".shards_mut(",
        ".alter_all(",
        ".try_get_mut(",
    ];
    let mut stack = vec![root];
    let mut offenders = Vec::new();
    while let Some(dir) = stack.pop() {
        let entries = std::fs::read_dir(&dir);
        assert!(entries.is_ok(), "read {}", dir.display());
        for path in entries
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
        {
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let name = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            if name.ends_with("tests.rs") || name == "write_hook.rs" || name == "cell_set.rs" {
                continue;
            }
            let source = std::fs::read_to_string(&path);
            assert!(source.is_ok(), "read {}", path.display());
            let compact: String = source
                .unwrap_or_default()
                .chars()
                .filter(|c| !c.is_whitespace())
                .collect();
            for mutator in mutators {
                if compact.contains(&format!(".data{mutator}")) {
                    offenders.push(format!("{}: .data{mutator}", path.display()));
                }
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "map data mutated outside CellMap::store/delete: {offenders:?}"
    );

    let mutation = include_str!("mutation.rs");
    let public: Vec<&str> = mutation
        .split("pub fn ")
        .skip(1)
        .filter_map(|rest| rest.split(['(', '<']).next())
        .collect();
    let covered = [
        "new",
        "own",
        "own_guard",
        "insert",
        "insert_many",
        "remove",
        "remove_many",
        "replace_all",
        "apply_diff_owned",
        "apply_batch",
        "lock",
    ];
    assert_eq!(
        public, covered,
        "a public CellMap method was added or removed: give any new mutator a write-hook test here"
    );
}

#[test]
fn concurrent_writers_keep_the_shadow_exact() {
    let map = CellMap::<u32, u32>::new();
    let shadow = Arc::new(Shadow::attach(&map));
    let writers = 8;
    std::thread::scope(|scope| {
        for writer in 0..writers {
            let (map, shadow) = (map.clone(), shadow.clone());
            scope.spawn(move || {
                for round in 0..2_000_u32 {
                    let key = writer * 10_000 + round % 64;
                    match round % 5 {
                        0 | 1 => drop(map.insert(key, round)),
                        2 => drop(map.insert(key, round % 3)),
                        3 => drop(map.remove(&key)),
                        _ => map.insert_many(vec![(key, round), (key + 1, round)]),
                    }
                    // This writer owns its keys, so the index must agree exactly.
                    for own in [key, key + 1] {
                        let indexed = shadow.rows.lock().get(&own).copied();
                        assert_eq!(indexed, map.get_value(&own));
                    }
                }
            });
        }
    });
    shadow.assert_matches(&map);
}

#[test]
fn readers_never_see_the_map_and_index_disagree_on_membership() {
    let map = CellMap::<u32, u32>::new();
    let shadow = Arc::new(Shadow::attach(&map));
    let done = AtomicBool::new(false);
    let keys = 20_000_u32;
    std::thread::scope(|scope| {
        for writer in 0..4 {
            let map = map.clone();
            scope.spawn(move || {
                for key in (writer..keys).step_by(4) {
                    map.insert(key, key);
                }
            });
        }
        for _ in 0..4 {
            let (map, shadow, done) = (map.clone(), shadow.clone(), &done);
            scope.spawn(move || {
                let mut key = 0;
                while !done.load(Ordering::Relaxed) {
                    key = (key + 7919) % keys;
                    // Map first: a visible row is already indexed.
                    if map.contains_key(&key) {
                        assert!(shadow.rows.lock().contains_key(&key));
                    }
                    // Index first: an indexed row is readable from the map.
                    if shadow.rows.lock().contains_key(&key) {
                        assert_eq!(map.get_value(&key), Some(key));
                    }
                }
            });
        }
        while map.snapshot().len() < usize::try_from(keys).unwrap_or(usize::MAX) {
            std::thread::yield_now();
        }
        done.store(true, Ordering::Relaxed);
    });
    shadow.assert_matches(&map);
}

#[test]
fn registering_during_concurrent_writes_seeds_without_gaps_or_doubles() {
    for _ in 0..20 {
        let map = CellMap::<u32, u32>::new();
        for key in 0..2_000 {
            map.insert(key, key);
        }
        let start = Barrier::new(5);
        let shadow = std::thread::scope(|scope| {
            for writer in 0..4_u32 {
                let (map, start) = (map.clone(), &start);
                scope.spawn(move || {
                    start.wait();
                    for round in 0..3_000_u32 {
                        let key = (writer * 3_000 + round) % 4_000;
                        if round % 3 == 0 {
                            map.remove(&key);
                        } else {
                            map.insert(key, round);
                        }
                    }
                });
            }
            start.wait();
            // Shadow::attach asserts on every event, so a seeded row that is
            // reported again, or a write the seed and the hook both miss,
            // fails here or in assert_matches below.
            Shadow::attach(&map)
        });
        shadow.assert_matches(&map);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CollidingKey(u8);

impl Hash for CollidingKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        0_u8.hash(state);
    }
}

/// A value whose Clone and Drop both read another key in the same shard.
struct Reentrant {
    value: u8,
    touch: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl Reentrant {
    fn new(value: u8, touch: &Arc<dyn Fn() + Send + Sync>) -> Self {
        Self {
            value,
            touch: Some(touch.clone()),
        }
    }
}

impl Clone for Reentrant {
    fn clone(&self) -> Self {
        if let Some(touch) = &self.touch {
            touch();
        }
        Self {
            value: self.value,
            touch: self.touch.clone(),
        }
    }
}

impl Drop for Reentrant {
    fn drop(&mut self) {
        if let Some(touch) = &self.touch {
            touch();
        }
    }
}

impl std::fmt::Debug for Reentrant {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.value.fmt(formatter)
    }
}

impl PartialEq for Reentrant {
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value
    }
}

#[test]
fn values_whose_clone_and_drop_touch_the_map_do_not_deadlock_with_a_hook() {
    let (finished_tx, finished_rx) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let map = CellMap::<CollidingKey, Reentrant>::new();
        let weak = map.downgrade();
        let touches = Arc::new(AtomicUsize::new(0));
        let counter = touches.clone();
        let touch: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            if let Some(map) = weak.upgrade() {
                let _ = map.get_value(&CollidingKey(0));
                counter.fetch_add(1, Ordering::Relaxed);
            }
        });
        map.insert(
            CollidingKey(0),
            Reentrant {
                value: 0,
                touch: None,
            },
        );
        let indexed = Arc::new(Mutex::new(Vec::new()));
        let log = indexed.clone();
        let guard = map.register_write_hook(move |write| {
            let key = match write {
                MapWrite::Insert { key, .. }
                | MapWrite::Update { key, .. }
                | MapWrite::Remove { key, .. } => key,
            };
            log.lock().push(key.0);
        });
        map.insert(CollidingKey(1), Reentrant::new(1, &touch));
        map.insert(CollidingKey(1), Reentrant::new(2, &touch));
        map.insert(CollidingKey(1), Reentrant::new(2, &touch));
        map.insert_many(vec![(CollidingKey(2), Reentrant::new(3, &touch))]);
        map.replace_all(vec![(
            CollidingKey(0),
            Reentrant {
                value: 0,
                touch: None,
            },
        )]);
        drop(guard);
        let _ = finished_tx.send((indexed.lock().clone(), touches.load(Ordering::Relaxed)));
    });
    let finished = finished_rx.recv_timeout(Duration::from_secs(10));
    assert!(
        finished.is_ok(),
        "a reentrant Clone or Drop deadlocked against a hooked write"
    );
    assert!(worker.join().is_ok());
    let (indexed, touches) = finished.unwrap_or_default();
    let (ordered, removed) = indexed.split_at(4);
    let mut removed = removed.to_vec();
    removed.sort_unstable();
    assert_eq!(ordered, [0, 1, 1, 2]);
    assert_eq!(
        removed,
        [1, 2],
        "replace_all removes keys 1 and 2 and leaves 0 equal"
    );
    assert!(touches > 0);
}

#[test]
fn dropping_a_hook_releases_its_captures_outside_the_shard_locks() {
    struct TouchOnDrop(crate::cell_map::WeakCellMap<u32, u32>);
    impl Drop for TouchOnDrop {
        fn drop(&mut self) {
            if let Some(map) = self.0.upgrade() {
                map.insert(1, 1);
            }
        }
    }
    let (done_tx, done_rx) = mpsc::channel();
    std::thread::spawn(move || {
        let map = CellMap::<u32, u32>::new();
        let capture = TouchOnDrop(map.downgrade());
        let guard = map.register_write_hook(move |_| {
            let _ = &capture;
        });
        drop(guard);
        let _ = done_tx.send(map.get_value(&1));
    });
    assert_eq!(done_rx.recv_timeout(Duration::from_secs(10)), Ok(Some(1)));
}

/// Writers racing on one key with two values: every equal write must be
/// suppressed and every real change reported with the exact old value, even
/// though the equality check is made before the write lock is taken.
#[test]
fn racing_writers_on_one_key_report_exact_changes() {
    let map = CellMap::<u32, u32>::new();
    let shadow = Shadow::attach(&map);
    std::thread::scope(|scope| {
        for writer in 0..4_u32 {
            let map = map.clone();
            scope.spawn(move || {
                for round in 0..20_000_u32 {
                    let value = (round.wrapping_mul(2_654_435_761) >> (writer + 7)) & 1;
                    map.insert(0, value);
                    if round % 97 == 0 {
                        map.remove(&0);
                    }
                }
            });
        }
    });
    shadow.assert_matches(&map);
}
