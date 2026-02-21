use std::{hash::Hash, marker::PhantomData, sync::Arc};

use dashmap::DashMap;

use crate::{
    cell::{CellImmutable, CellMutable},
    cell_map::{CellMap, MapDiff},
    signal::Signal,
    subscription::SubscriptionGuard,
    traits::{CellValue, Gettable, Watchable, collections::internal::map_runtime::flatten_diff},
};

/// Maps each row value through a reactive cell/watchable and keeps the same key.
///
/// `mapper(&key, &value)` returns a watchable value for that row; output updates whenever
/// that watchable emits.
#[track_caller]
pub(crate) fn map_values_cell<K, V, M, U, W, F>(
    source: &CellMap<K, V, M>,
    mapper: F,
) -> CellMap<K, U, CellImmutable>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
    U: CellValue,
    W: Watchable<U> + Gettable<U> + Clone + Send + Sync + 'static,
    F: Fn(&K, &V) -> W + Send + Sync + 'static,
{
    let mapped = CellMap::<K, U, CellMutable>::new();
    if let Some(parent_name) = (**source.inner.name.load()).as_ref() {
        mapped
            .clone()
            .with_name(format!("{}::map_values_cell", parent_name));
    }

    let mapper = Arc::new(mapper);
    let per_key_guards = Arc::new(DashMap::<K, SubscriptionGuard>::new());
    let mapped_weak = Arc::downgrade(&mapped.inner);

    let guard = source.subscribe_diffs(move |diff| {
        let Some(inner) = mapped_weak.upgrade() else {
            return;
        };
        let mapped = CellMap::<K, U, CellMutable> {
            inner,
            _marker: PhantomData,
        };

        let attach = |key: K, value: V, mut batch_changes: Option<&mut Vec<MapDiff<K, U>>>| {
            if let Some((_, old_guard)) = per_key_guards.remove(&key) {
                drop(old_guard);
            }

            let inner_cell = mapper(&key, &value);
            let suppress_initial = batch_changes.is_some();
            if let Some(changes) = batch_changes.as_deref_mut() {
                let initial = inner_cell.get();
                if let Some(old_value) = mapped.get_value(&key) {
                    changes.push(MapDiff::Update {
                        key: key.clone(),
                        old_value,
                        new_value: initial,
                    });
                } else {
                    changes.push(MapDiff::Insert {
                        key: key.clone(),
                        value: initial,
                    });
                }
            }
            let key_for_sub = key.clone();
            let mapped_weak_for_sub = Arc::downgrade(&mapped.inner);
            let first = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
            let first_for_sub = first.clone();
            let sub_guard = inner_cell.subscribe(move |signal| {
                let Some(inner) = mapped_weak_for_sub.upgrade() else {
                    return;
                };
                let mapped = CellMap::<K, U, CellMutable> {
                    inner,
                    _marker: PhantomData,
                };

                if let Signal::Value(v) = signal {
                    if suppress_initial
                        && first_for_sub.swap(false, std::sync::atomic::Ordering::SeqCst)
                    {
                        return;
                    }
                    mapped.insert(key_for_sub.clone(), (**v).clone());
                }
            });
            per_key_guards.insert(key, sub_guard);
        };

        let detach = |key: &K, batch_changes: Option<&mut Vec<MapDiff<K, U>>>| {
            if let Some((_, old_guard)) = per_key_guards.remove(key) {
                drop(old_guard);
            }
            if let Some(changes) = batch_changes {
                if let Some(old_value) = mapped.get_value(key) {
                    changes.push(MapDiff::Remove {
                        key: key.clone(),
                        old_value,
                    });
                }
            } else {
                mapped.remove(key);
            }
        };

        match diff {
            MapDiff::Initial { entries } => {
                let existing_keys: Vec<K> = per_key_guards
                    .iter()
                    .map(|entry| entry.key().clone())
                    .collect();
                for k in existing_keys {
                    detach(&k, None);
                }
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
                let mut downstream_changes: Vec<MapDiff<K, U>> = Vec::new();
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
                mapped.apply_batch(downstream_changes);
            }
        }
    });

    mapped.own(guard);
    mapped.lock()
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;
    use crate::{Cell, MapExt};

    #[test]
    fn map_values_cell_reacts_per_row() {
        let values = CellMap::<String, i32>::new();
        let factors = CellMap::<String, i32>::new();

        values.insert("a".to_string(), 10);
        values.insert("b".to_string(), 20);
        factors.insert("a".to_string(), 1);
        factors.insert("b".to_string(), 2);

        let out = map_values_cell(&values, {
            let factors = factors.clone();
            move |key, value| {
                let v = *value;
                factors.get(key).map(move |f| v * f.unwrap_or(0))
            }
        });

        assert_eq!(out.get_value(&"a".to_string()), Some(10));
        assert_eq!(out.get_value(&"b".to_string()), Some(40));
        factors.insert("a".to_string(), 3);
        assert_eq!(out.get_value(&"a".to_string()), Some(30));
    }

    #[test]
    fn map_values_cell_preserves_upstream_batch_without_extra_emissions() {
        let source = CellMap::<String, i32>::new();
        let out = map_values_cell(&source, |_, v| Cell::new(*v * 10).lock());

        let (tx, rx) = mpsc::channel::<MapDiff<String, i32>>();
        let _guard = out.subscribe_diffs(move |diff| {
            let _ = tx.send(diff.clone());
        });

        source.insert_many(vec![("a".to_string(), 1), ("b".to_string(), 2)]);
        let seen: Vec<_> = rx.try_iter().collect();
        assert_eq!(seen.len(), 2);
        match seen.last().unwrap() {
            MapDiff::Batch { changes } => assert_eq!(changes.len(), 2),
            _ => panic!("expected batch diff from map_values_cell"),
        }
    }
}
