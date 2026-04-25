use std::hash::Hash;

use super::ProjectCellExt;
use crate::{
    cell::CellImmutable,
    cell_map::CellMap,
    map_query::MapQuery,
    pipeline::Pipeline,
    traits::{CellValue, Gettable, MapExt, Watchable},
};

pub trait SelectCellExt<K, V>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    /// Reactive filter: each row has a watchable boolean gate.
    ///
    /// `predicate(&key, &value)` returns a watchable bool; the row is included when true and
    /// excluded when false.
    #[track_caller]
    fn select_cell<W, F>(&self, predicate: F) -> CellMap<K, V, CellImmutable>
    where
        W: Watchable<bool> + Gettable<bool> + Clone + Send + Sync + 'static,
        F: Fn(&K, &V) -> W + Send + Sync + 'static;
}

impl<K, V, M> SelectCellExt<K, V> for CellMap<K, V, M>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
    M: Clone + Send + Sync + 'static,
{
    fn select_cell<W, F>(&self, predicate: F) -> CellMap<K, V, CellImmutable>
    where
        W: Watchable<bool> + Gettable<bool> + Clone + Send + Sync + 'static,
        F: Fn(&K, &V) -> W + Send + Sync + 'static,
    {
        self.clone()
            .project_cell(move |k, v| {
                let k = k.clone();
                let v = v.clone();
                predicate(&k, &v)
                    .map(move |include| {
                        if *include {
                            Some((k.clone(), v.clone()))
                        } else {
                            None
                        }
                    })
                    .materialize()
            })
            .materialize()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;
    use crate::{Cell, MapExt, cell_map::MapDiff, pipeline::Pipeline};

    #[test]
    fn select_cell_reacts_to_predicate_changes() {
        let values = CellMap::<String, i32>::new();
        let gates = CellMap::<String, bool>::new();

        values.insert("a".to_string(), 10);
        values.insert("b".to_string(), 20);
        gates.insert("a".to_string(), false);
        gates.insert("b".to_string(), true);

        let filtered = values.select_cell({
            let gates = gates.clone();
            move |key, _value| gates.get(key).map(|v| v.unwrap_or(false)).materialize()
        });

        assert_eq!(filtered.entries().get().len(), 1);
        assert!(!filtered.contains_key(&"a".to_string()));
        assert!(filtered.contains_key(&"b".to_string()));

        gates.insert("a".to_string(), true);
        assert_eq!(filtered.entries().get().len(), 2);
        gates.insert("b".to_string(), false);
        assert_eq!(filtered.entries().get().len(), 1);
    }

    #[test]
    fn select_cell_preserves_upstream_batch_without_extra_emissions() {
        let source = CellMap::<String, i32>::new();
        let out = source.select_cell(|_, _| Cell::new(true).lock());

        let (tx, rx) = mpsc::channel::<MapDiff<String, i32>>();
        let _guard = out.subscribe_diffs(move |diff| {
            let _ = tx.send(diff.clone());
        });

        source.insert_many(vec![("a".to_string(), 1), ("b".to_string(), 2)]);
        let seen: Vec<_> = rx.try_iter().collect();
        assert_eq!(seen.len(), 2);
        match seen.last().unwrap() {
            MapDiff::Batch { changes } => assert_eq!(changes.len(), 2),
            _ => panic!("expected batch diff from select_cell"),
        }
    }
}
