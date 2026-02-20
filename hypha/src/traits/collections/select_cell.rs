use std::{hash::Hash, marker::PhantomData, sync::Arc};

use dashmap::DashMap;

use crate::{
    cell::{CellImmutable, CellMutable},
    cell_map::{CellMap, MapDiff},
    signal::Signal,
    subscription::SubscriptionGuard,
    traits::{CellValue, Gettable, Watchable, collections::internal::map_runtime::flatten_diff},
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
{
    fn select_cell<W, F>(&self, predicate: F) -> CellMap<K, V, CellImmutable>
    where
        W: Watchable<bool> + Gettable<bool> + Clone + Send + Sync + 'static,
        F: Fn(&K, &V) -> W + Send + Sync + 'static,
    {
        let filtered = CellMap::<K, V, CellMutable>::new();
        if let Some(parent_name) = (**self.inner.name.load()).as_ref() {
            filtered
                .clone()
                .with_name(format!("{}::select_cell", parent_name));
        }

        let predicate = Arc::new(predicate);
        let per_key_guards = Arc::new(DashMap::<K, SubscriptionGuard>::new());

        let filtered_weak = Arc::downgrade(&filtered.inner);
        let guard = self.subscribe_diffs(move |diff| {
            let Some(inner) = filtered_weak.upgrade() else {
                return;
            };
            let filtered = CellMap::<K, V, CellMutable> {
                inner,
                _marker: PhantomData,
            };

            let attach = |key: K, value: V, mut batch_changes: Option<&mut Vec<MapDiff<K, V>>>| {
                if let Some((_, old_guard)) = per_key_guards.remove(&key) {
                    drop(old_guard);
                }

                let include_cell = predicate(&key, &value);
                let suppress_initial = batch_changes.is_some();
                if let Some(changes) = batch_changes.as_deref_mut() {
                    let include = include_cell.get();
                    let current = filtered.get_value(&key);
                    if include {
                        if let Some(old_value) = current {
                            changes.push(MapDiff::Update {
                                key: key.clone(),
                                old_value,
                                new_value: value.clone(),
                            });
                        } else {
                            changes.push(MapDiff::Insert {
                                key: key.clone(),
                                value: value.clone(),
                            });
                        }
                    } else if let Some(old_value) = current {
                        changes.push(MapDiff::Remove {
                            key: key.clone(),
                            old_value,
                        });
                    }
                }

                let key_for_sub = key.clone();
                let value_for_sub = value.clone();
                let filtered_weak_for_sub = Arc::downgrade(&filtered.inner);
                let first = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
                let first_for_sub = first.clone();

                let sub_guard = include_cell.subscribe(move |include_signal| {
                    let Some(inner) = filtered_weak_for_sub.upgrade() else {
                        return;
                    };
                    let filtered = CellMap::<K, V, CellMutable> {
                        inner,
                        _marker: PhantomData,
                    };

                    if let Signal::Value(include) = include_signal {
                        if suppress_initial
                            && first_for_sub.swap(false, std::sync::atomic::Ordering::SeqCst)
                        {
                            return;
                        }
                        if **include {
                            filtered.insert(key_for_sub.clone(), value_for_sub.clone());
                        } else {
                            filtered.remove(&key_for_sub);
                        }
                    }
                });

                per_key_guards.insert(key, sub_guard);
            };

            let detach = |key: &K, batch_changes: Option<&mut Vec<MapDiff<K, V>>>| {
                if let Some((_, old_guard)) = per_key_guards.remove(key) {
                    drop(old_guard);
                }
                if let Some(changes) = batch_changes {
                    if let Some(old_value) = filtered.get_value(key) {
                        changes.push(MapDiff::Remove {
                            key: key.clone(),
                            old_value,
                        });
                    }
                } else {
                    filtered.remove(key);
                }
            };

            match diff {
                MapDiff::Initial { entries } => {
                    for (key, value) in entries {
                        attach(key.clone(), value.clone(), None);
                    }
                }
                MapDiff::Insert { key, value } => {
                    attach(key.clone(), value.clone(), None);
                }
                MapDiff::Remove { key, .. } => {
                    detach(key, None);
                }
                MapDiff::Update { key, new_value, .. } => {
                    attach(key.clone(), new_value.clone(), None);
                }
                MapDiff::Batch { changes } => {
                    let mut downstream_changes: Vec<MapDiff<K, V>> = Vec::new();
                    let mut atomic_changes: Vec<MapDiff<K, V>> = Vec::new();
                    for change in changes {
                        flatten_diff(change, &mut atomic_changes);
                    }
                    for change in &atomic_changes {
                        match change {
                            MapDiff::Initial { entries } => {
                                let existing_keys: Vec<K> = per_key_guards
                                    .iter()
                                    .map(|entry| entry.key().clone())
                                    .collect();
                                for k in existing_keys {
                                    detach(&k, Some(&mut downstream_changes));
                                }
                                for (key, value) in entries {
                                    attach(
                                        key.clone(),
                                        value.clone(),
                                        Some(&mut downstream_changes),
                                    );
                                }
                            }
                            MapDiff::Insert { key, value } => {
                                attach(key.clone(), value.clone(), Some(&mut downstream_changes));
                            }
                            MapDiff::Remove { key, .. } => {
                                detach(key, Some(&mut downstream_changes));
                            }
                            MapDiff::Update { key, new_value, .. } => {
                                attach(
                                    key.clone(),
                                    new_value.clone(),
                                    Some(&mut downstream_changes),
                                );
                            }
                            MapDiff::Batch { .. } => {
                                unreachable!("flatten_diff emits atomic changes")
                            }
                        }
                    }
                    filtered.apply_batch(downstream_changes);
                }
            }
        });

        filtered.own(guard);
        filtered.lock()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;
    use crate::{Cell, MapExt};

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
            move |key, _value| gates.get(key).map(|v| v.unwrap_or(false))
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
