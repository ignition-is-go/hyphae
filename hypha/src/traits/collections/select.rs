use std::hash::Hash;

use crate::{
    cell::CellImmutable,
    cell_map::CellMap,
    traits::{CellValue, collections::internal::map_runtime::run_map_runtime},
};

pub trait SelectExt<K, V>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    /// Filters rows by value.
    ///
    /// `predicate(&value)` decides whether a row is present in the output map.
    #[track_caller]
    fn select<F>(&self, predicate: F) -> CellMap<K, V, CellImmutable>
    where
        F: Fn(&V) -> bool + Send + Sync + 'static;
}

impl<K, V, M> SelectExt<K, V> for CellMap<K, V, M>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    fn select<F>(&self, predicate: F) -> CellMap<K, V, CellImmutable>
    where
        F: Fn(&V) -> bool + Send + Sync + 'static,
    {
        run_map_runtime(self, "select", move |k, v| {
            if predicate(v) {
                vec![(k.clone(), v.clone())]
            } else {
                Vec::new()
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    };

    use super::*;
    use crate::{
        MapDiff,
        traits::{Gettable, Watchable},
    };

    #[test]
    fn select_filters_and_updates() {
        let map = CellMap::<String, i32>::new();
        map.insert("a".to_string(), 5);
        map.insert("b".to_string(), 15);
        map.insert("c".to_string(), 25);

        let filtered = map.select(|v| *v > 10);
        assert_eq!(filtered.entries().get().len(), 2);
        assert!(filtered.contains_key(&"b".to_string()));
        assert!(filtered.contains_key(&"c".to_string()));
        assert!(!filtered.contains_key(&"a".to_string()));

        map.insert("a".to_string(), 30);
        assert!(filtered.contains_key(&"a".to_string()));
        map.insert("b".to_string(), 1);
        assert!(!filtered.contains_key(&"b".to_string()));
    }

    #[test]
    fn select_batch_resilience_and_no_extra_side_emissions() {
        let map = CellMap::<String, i32>::new();
        let filtered = map.select(|v| *v > 10);

        let (tx, rx) = mpsc::channel::<MapDiff<String, i32>>();
        let _guard = filtered.subscribe_diffs(move |diff| {
            let _ = tx.send(diff.clone());
        });

        map.insert_many(vec![("a".to_string(), 15), ("b".to_string(), 20)]);

        let seen: Vec<_> = rx.try_iter().collect();
        assert_eq!(seen.len(), 2);
        match seen.last().unwrap() {
            MapDiff::Batch { changes } => assert_eq!(changes.len(), 2),
            _ => panic!("expected batch diff from select"),
        }

        let before = filtered.entries().get().len();
        map.insert_many(vec![("x".to_string(), 1), ("y".to_string(), 2)]);
        let after = filtered.entries().get().len();
        assert_eq!(before, after);
    }

    #[test]
    fn select_entries_observable() {
        let map = CellMap::<String, i32>::new();
        let filtered = map.select(|v| *v > 10);
        let entries = filtered.entries();

        let count = Arc::new(AtomicUsize::new(0));
        let c = count.clone();
        let _guard = entries.subscribe(move |_| {
            c.fetch_add(1, Ordering::SeqCst);
        });

        assert_eq!(count.load(Ordering::SeqCst), 1);
        map.insert("a".to_string(), 15);
        assert_eq!(count.load(Ordering::SeqCst), 2);
        map.insert("b".to_string(), 5);
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }
}
