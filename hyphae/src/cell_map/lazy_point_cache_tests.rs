use std::sync::mpsc;

use super::*;
use crate::traits::Gettable;

#[test]
fn first_points_reflect_prior_mutations_and_keep_receiving_updates() {
    let map = CellMap::<u8, u8>::new();
    let entries = map.entries().materialize();
    let keys = map.keys().materialize();
    let items = map.items().materialize();
    let length = map.len().materialize();
    map.insert(1, 10);
    map.insert_many(vec![(1, 11), (2, 20), (3, 30)]);
    assert_eq!(map.remove(&1), Some(11));
    map.remove_many(vec![3]);
    map.replace_all(vec![(2, 22), (4, 40)]);

    let mut snapshot = map.snapshot();
    snapshot.sort_unstable();
    assert_eq!(snapshot, vec![(2, 22), (4, 40)]);
    let mut observed_entries = entries.get();
    observed_entries.sort_unstable();
    assert_eq!(observed_entries, vec![(2, 22), (4, 40)]);
    let mut observed_keys = keys.get();
    observed_keys.sort_unstable();
    assert_eq!(observed_keys, vec![2, 4]);
    let mut observed_items = items.get();
    observed_items.sort_unstable();
    assert_eq!(observed_items, vec![22, 40]);
    assert_eq!(length.get(), 2);

    let existing = map.get(&2).materialize();
    let removed = map.get(&1).materialize();
    let missing = map.get(&9).materialize();
    assert_eq!(existing.get(), Some(22));
    assert_eq!(removed.get(), None);
    assert_eq!(missing.get(), None);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let observed = seen.clone();
    let _guard = existing.subscribe(move |signal| {
        if let Signal::Value(value) = signal {
            observed.lock().push(**value);
        }
    });
    map.insert_many(vec![(2, 23), (1, 12), (9, 90)]);
    assert_eq!(existing.get(), Some(23));
    assert_eq!(removed.get(), Some(12));
    assert_eq!(missing.get(), Some(90));
    map.remove(&2);
    assert_eq!(existing.get(), None);
    assert_eq!(*seen.lock(), vec![Some(22), Some(23), None]);
}

struct CloneReadingKey {
    value: u8,
    read: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl Clone for CloneReadingKey {
    fn clone(&self) -> Self {
        if let Some(read) = &self.read {
            read();
        }
        Self {
            value: self.value,
            read: self.read.clone(),
        }
    }
}

impl std::fmt::Debug for CloneReadingKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&self.value, formatter)
    }
}

impl PartialEq for CloneReadingKey {
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value
    }
}

impl Eq for CloneReadingKey {}

impl Hash for CloneReadingKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        0_u8.hash(state);
    }
}

#[test]
fn first_point_key_clone_can_materialize_another_point() {
    let map = CellMap::<CloneReadingKey, u8>::new();
    map.insert(
        CloneReadingKey {
            value: 2,
            read: None,
        },
        20,
    );
    let weak = map.downgrade();
    let (nested_tx, nested_rx) = mpsc::channel();
    let key = CloneReadingKey {
        value: 1,
        read: Some(Arc::new(move || {
            let Some(map) = weak.upgrade() else {
                std::panic::resume_unwind(Box::new("map dropped during key clone"));
            };
            let point = map
                .get(&CloneReadingKey {
                    value: 2,
                    read: None,
                })
                .materialize();
            assert_eq!(point.get(), Some(20));
            assert!(nested_tx.send(point).is_ok());
        })),
    };
    let source = map.clone();
    let (finished_tx, finished_rx) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        assert!(finished_tx.send(source.get(&key).materialize()).is_ok());
    });
    let finished = finished_rx.recv_timeout(std::time::Duration::from_secs(5));
    assert!(finished.is_ok());
    let Ok(point) = finished else { return };
    assert!(worker.join().is_ok());
    assert_eq!(point.get(), None);
    let nested = nested_rx.recv_timeout(std::time::Duration::from_secs(5));
    assert!(nested.is_ok());
    let Ok(nested) = nested else { return };
    map.insert(
        CloneReadingKey {
            value: 1,
            read: None,
        },
        10,
    );
    map.insert(
        CloneReadingKey {
            value: 2,
            read: None,
        },
        21,
    );
    assert_eq!(point.get(), Some(10));
    assert_eq!(nested.get(), Some(21));
}
