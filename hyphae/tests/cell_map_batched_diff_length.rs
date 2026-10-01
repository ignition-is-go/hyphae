#![cfg(feature = "scheduler")]

use std::sync::Arc;

use hyphae::{CellMap, Gettable, MapDiff, Materialize};
use parking_lot::Mutex;

#[test]
fn single_diff_length_uses_membership_changes_when_cell_writes_are_deferred() {
    let map = CellMap::<String, i32>::new();
    let length = map.len().materialize();
    let entries = map.entries().materialize();
    let diffs = Arc::new(Mutex::new(Vec::new()));
    let observed = diffs.clone();
    let _guard = map.subscribe_diffs(move |diff| observed.lock().push(diff.clone()));
    let changes = vec![
        MapDiff::Insert {
            key: "a".to_owned(),
            value: 1,
        },
        MapDiff::Update {
            key: "a".to_owned(),
            old_value: 1,
            new_value: 2,
        },
        MapDiff::Remove {
            key: "a".to_owned(),
            old_value: 2,
        },
        MapDiff::Update {
            key: "b".to_owned(),
            old_value: 0,
            new_value: 3,
        },
        MapDiff::Remove {
            key: "b".to_owned(),
            old_value: 3,
        },
    ];
    hyphae::batch(|| {
        for diff in &changes {
            map.apply_diff_owned(diff.clone());
        }
    });
    assert_eq!(length.get(), 0);
    assert!(entries.get().is_empty());
    let mut expected = vec![MapDiff::Initial {
        entries: Vec::new(),
    }];
    expected.extend(changes);
    assert_eq!(*diffs.lock(), expected);
}
