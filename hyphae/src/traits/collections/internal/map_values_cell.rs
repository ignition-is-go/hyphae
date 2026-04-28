use std::{
    collections::HashMap,
    hash::Hash,
    sync::{Arc, Mutex},
};

use dashmap::DashMap;

use crate::{
    cell_map::MapDiff,
    signal::Signal,
    subscription::SubscriptionGuard,
    traits::{CellValue, Gettable, Watchable, collections::internal::map_runtime::flatten_diff},
};

/// Sink-driven core for `map_values_cell` / `project_cell`.
///
/// For each source row, installs a per-row [`Watchable`] subscription whose
/// emissions are pushed as [`MapDiff`]s into `sink`. Tracks the latest emitted
/// `U` per source key so it can construct proper Insert/Update/Remove diffs
/// without requiring an intermediate [`CellMap`].
///
/// The runtime is generic in `U`; project_cell's `Option<(K2, V2)>` projection
/// step lives in [`crate::traits::collections::project_cell`] as a follow-on
/// stage on top of this primitive.
pub(crate) fn install_map_values_cell_via_query<K, V, U, S, W, F>(
    source: S,
    mapper: F,
    sink: crate::map_query::MapDiffSink<K, U>,
) -> Vec<SubscriptionGuard>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
    U: CellValue,
    S: crate::map_query::MapQuery<K, V>,
    W: Watchable<U> + Gettable<U> + Clone + Send + Sync + 'static,
    F: Fn(&K, &V) -> W + Send + Sync + 'static,
{
    let mapper = Arc::new(mapper);
    let per_key_guards = Arc::new(DashMap::<K, SubscriptionGuard>::new());
    // Tracks the most recent `U` we've emitted for each source key so we can
    // produce proper Insert/Update/Remove diffs without a backing CellMap.
    let last_value: Arc<Mutex<HashMap<K, U>>> = Arc::new(Mutex::new(HashMap::new()));

    let upstream_sink: crate::map_query::MapDiffSink<K, V> = {
        let mapper = mapper.clone();
        let per_key_guards = per_key_guards.clone();
        let last_value = last_value.clone();
        let sink = sink.clone();
        Arc::new(move |diff| {
            // attach: install (or replace) the per-row Watchable subscription
            // for `key`. Emits a single diff (Insert or Update) into either
            // the batch buffer or directly into the sink, then wires the
            // Watchable's subscription to emit Update on each subsequent
            // emission.
            let attach = |key: K,
                          value: V,
                          mut batch_changes: Option<&mut Vec<MapDiff<K, U>>>| {
                // Drop any previous per-row guard before installing a new one.
                if let Some((_, old_guard)) = per_key_guards.remove(&key) {
                    drop(old_guard);
                }

                let inner_cell = mapper(&key, &value);
                let initial = inner_cell.get();

                // Emit the initial diff for this row (Insert if new, Update if
                // we already had a value tracked).
                let prior = {
                    let mut last = last_value.lock().unwrap_or_else(|e| e.into_inner());
                    last.insert(key.clone(), initial.clone())
                };
                let diff_out = match prior {
                    Some(old_value) => MapDiff::Update {
                        key: key.clone(),
                        old_value,
                        new_value: initial,
                    },
                    None => MapDiff::Insert {
                        key: key.clone(),
                        value: initial,
                    },
                };

                if let Some(changes) = batch_changes.as_deref_mut() {
                    changes.push(diff_out);
                } else {
                    sink(&diff_out);
                }

                // Wire the Watchable's ongoing emissions to the sink. The
                // first emission from `subscribe` is the current value, which
                // we already emitted above; suppress it.
                let key_for_sub = key.clone();
                let last_value_for_sub = last_value.clone();
                let sink_for_sub = sink.clone();
                let first =
                    std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
                let sub_guard = inner_cell.subscribe(move |signal| {
                    if let Signal::Value(v) = signal {
                        if first.swap(false, std::sync::atomic::Ordering::SeqCst) {
                            return;
                        }
                        let new_value: U = (**v).clone();
                        let prior = {
                            let mut last = last_value_for_sub
                                .lock()
                                .unwrap_or_else(|e| e.into_inner());
                            last.insert(key_for_sub.clone(), new_value.clone())
                        };
                        let diff_out = match prior {
                            Some(old_value) => {
                                if old_value == new_value {
                                    return;
                                }
                                MapDiff::Update {
                                    key: key_for_sub.clone(),
                                    old_value,
                                    new_value,
                                }
                            }
                            None => MapDiff::Insert {
                                key: key_for_sub.clone(),
                                value: new_value,
                            },
                        };
                        sink_for_sub(&diff_out);
                    }
                });
                per_key_guards.insert(key, sub_guard);
            };

            // detach: drop the per-row guard and emit Remove for the row's
            // last emitted value (if any).
            let detach = |key: &K, batch_changes: Option<&mut Vec<MapDiff<K, U>>>| {
                if let Some((_, old_guard)) = per_key_guards.remove(key) {
                    drop(old_guard);
                }
                let prior = {
                    let mut last = last_value.lock().unwrap_or_else(|e| e.into_inner());
                    last.remove(key)
                };
                if let Some(old_value) = prior {
                    let diff_out = MapDiff::Remove {
                        key: key.clone(),
                        old_value,
                    };
                    if let Some(changes) = batch_changes {
                        changes.push(diff_out);
                    } else {
                        sink(&diff_out);
                    }
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
                                attach(
                                    key.clone(),
                                    value.clone(),
                                    Some(&mut downstream_changes),
                                );
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
                    if !downstream_changes.is_empty() {
                        sink(&MapDiff::Batch {
                            changes: downstream_changes,
                        });
                    }
                }
            }
        })
    };

    source.install(upstream_sink)
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;
    use crate::{MaterializeDefinite, Cell, CellMap, MapExt, cell_map::MapDiff};

    /// Capture every diff emitted into a `MapDiffSink` and replay them onto
    /// an in-memory `HashMap<K, U>` to assert per-row state.
    fn capturing_sink<K, U>() -> (
        crate::map_query::MapDiffSink<K, U>,
        Arc<Mutex<HashMap<K, U>>>,
        Arc<Mutex<Vec<MapDiff<K, U>>>>,
    )
    where
        K: Hash + Eq + CellValue,
        U: CellValue,
    {
        let state: Arc<Mutex<HashMap<K, U>>> = Arc::new(Mutex::new(HashMap::new()));
        let diffs: Arc<Mutex<Vec<MapDiff<K, U>>>> = Arc::new(Mutex::new(Vec::new()));
        let sink: crate::map_query::MapDiffSink<K, U> = {
            let state = state.clone();
            let diffs = diffs.clone();
            Arc::new(move |diff| {
                let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
                apply_to_hashmap(&mut state, diff);
                diffs.lock().unwrap_or_else(|e| e.into_inner()).push(diff.clone());
            })
        };
        (sink, state, diffs)
    }

    fn apply_to_hashmap<K, U>(state: &mut HashMap<K, U>, diff: &MapDiff<K, U>)
    where
        K: Hash + Eq + CellValue,
        U: CellValue,
    {
        match diff {
            MapDiff::Initial { entries } => {
                state.clear();
                for (k, v) in entries {
                    state.insert(k.clone(), v.clone());
                }
            }
            MapDiff::Insert { key, value } => {
                state.insert(key.clone(), value.clone());
            }
            MapDiff::Update { key, new_value, .. } => {
                state.insert(key.clone(), new_value.clone());
            }
            MapDiff::Remove { key, .. } => {
                state.remove(key);
            }
            MapDiff::Batch { changes } => {
                for change in changes {
                    apply_to_hashmap(state, change);
                }
            }
        }
    }

    #[test]
    fn map_values_cell_reacts_per_row() {
        let values = CellMap::<String, i32>::new();
        let factors = CellMap::<String, i32>::new();

        values.insert("a".to_string(), 10);
        values.insert("b".to_string(), 20);
        factors.insert("a".to_string(), 1);
        factors.insert("b".to_string(), 2);

        let (sink, state, _diffs) = capturing_sink::<String, i32>();
        let _guards = install_map_values_cell_via_query(
            values.clone(),
            {
                let factors = factors.clone();
                move |key: &String, value: &i32| {
                    let v = *value;
                    factors
                        .get(key)
                        .map(move |f| v * f.unwrap_or(0))
                        .materialize()
                }
            },
            sink,
        );

        assert_eq!(
            state.lock().unwrap().get(&"a".to_string()).copied(),
            Some(10)
        );
        assert_eq!(
            state.lock().unwrap().get(&"b".to_string()).copied(),
            Some(40)
        );
        factors.insert("a".to_string(), 3);
        assert_eq!(
            state.lock().unwrap().get(&"a".to_string()).copied(),
            Some(30)
        );
    }

    #[test]
    fn map_values_cell_preserves_upstream_batch_without_extra_emissions() {
        let source = CellMap::<String, i32>::new();

        let (tx, rx) = mpsc::channel::<MapDiff<String, i32>>();
        let sink: crate::map_query::MapDiffSink<String, i32> = Arc::new(move |diff| {
            let _ = tx.send(diff.clone());
        });

        let _guards = install_map_values_cell_via_query(
            source.clone(),
            |_: &String, v: &i32| Cell::new(*v * 10).lock(),
            sink,
        );

        // Drain the initial empty diff emitted on subscription.
        let _ = rx.try_iter().count();

        source.insert_many(vec![("a".to_string(), 1), ("b".to_string(), 2)]);
        let seen: Vec<_> = rx.try_iter().collect();
        assert_eq!(seen.len(), 1);
        match seen.last().unwrap() {
            MapDiff::Batch { changes } => assert_eq!(changes.len(), 2),
            _ => panic!("expected batch diff from install_map_values_cell_via_query"),
        }
    }
}
