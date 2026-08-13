use std::{
    collections::HashMap,
    hash::Hash,
    marker::PhantomData,
    sync::{Arc, Mutex},
};

use dashmap::DashMap;

use crate::{
    cell_map::MapDiff,
    signal::Signal,
    subscription::SubscriptionGuard,
    traits::{CellValue, Gettable, Watchable, collections::internal::map_runtime::flatten_diff},
};

struct MapValuesRuntime<K, V, U, W, F>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
    U: CellValue,
    W: Watchable<U> + Gettable<U> + Clone + Send + Sync + 'static,
    F: Fn(&K, &V) -> W + Send + Sync + 'static,
{
    mapper: Arc<F>,
    per_key_guards: Arc<DashMap<K, SubscriptionGuard>>,
    last_value: Arc<Mutex<HashMap<K, U>>>,
    sink: crate::map_query::BoxedMapDiffSink<K, U>,
    marker: PhantomData<fn(V) -> W>,
}

impl<K, V, U, W, F> MapValuesRuntime<K, V, U, W, F>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
    U: CellValue,
    W: Watchable<U> + Gettable<U> + Clone + Send + Sync + 'static,
    F: Fn(&K, &V) -> W + Send + Sync + 'static,
{
    fn emit(&self, diff: MapDiff<K, U>, changes: Option<&mut Vec<MapDiff<K, U>>>) {
        if let Some(changes) = changes {
            changes.push(diff);
        } else {
            (self.sink)(&diff);
        }
    }

    fn attach(&self, key: K, value: &V, changes: Option<&mut Vec<MapDiff<K, U>>>) {
        if let Some((_, old_guard)) = self.per_key_guards.remove(&key) {
            drop(old_guard);
        }

        let inner_cell = (self.mapper)(&key, value);
        let initial = inner_cell.get();
        let prior = {
            let mut last = self
                .last_value
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            last.insert(key.clone(), initial.clone())
        };
        let diff = match prior {
            None => MapDiff::Insert {
                key: key.clone(),
                value: initial,
            },
            Some(old_value) => MapDiff::Update {
                key: key.clone(),
                old_value,
                new_value: initial,
            },
        };
        self.emit(diff, changes);

        let key_for_sub = key.clone();
        let last_value = Arc::clone(&self.last_value);
        let sink = Arc::clone(&self.sink);
        let first = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let sub_guard = inner_cell.subscribe(move |signal| {
            if let Signal::Value(value) = signal {
                if first.swap(false, std::sync::atomic::Ordering::SeqCst) {
                    return;
                }
                let new_value: U = (**value).clone();
                let prior = {
                    let mut last = last_value
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    last.insert(key_for_sub.clone(), new_value.clone())
                };
                let diff = match prior {
                    None => MapDiff::Insert {
                        key: key_for_sub.clone(),
                        value: new_value,
                    },
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
                };
                sink(&diff);
            }
        });
        self.per_key_guards.insert(key, sub_guard);
    }

    fn detach(&self, key: &K, changes: Option<&mut Vec<MapDiff<K, U>>>) {
        if let Some((_, old_guard)) = self.per_key_guards.remove(key) {
            drop(old_guard);
        }
        let prior = {
            let mut last = self
                .last_value
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            last.remove(key)
        };
        if let Some(old_value) = prior {
            self.emit(
                MapDiff::Remove {
                    key: key.clone(),
                    old_value,
                },
                changes,
            );
        }
    }

    fn apply(&self, diff: &MapDiff<K, V>, mut changes: Option<&mut Vec<MapDiff<K, U>>>) {
        match diff {
            MapDiff::Initial { entries } => {
                let existing_keys: Vec<K> = self
                    .per_key_guards
                    .iter()
                    .map(|entry| entry.key().clone())
                    .collect();
                for key in existing_keys {
                    self.detach(&key, changes.as_deref_mut());
                }
                for (key, value) in entries {
                    self.attach(key.clone(), value, changes.as_deref_mut());
                }
            }
            MapDiff::Insert { key, value } => {
                self.attach(key.clone(), value, changes);
            }
            MapDiff::Remove { key, .. } => self.detach(key, changes),
            MapDiff::Update { key, new_value, .. } => {
                self.attach(key.clone(), new_value, changes);
            }
            MapDiff::Batch { .. } => {}
        }
    }
}

/// Sink-driven core for `map_values_cell` / `project_cell`.
///
/// For each source row, installs a per-row [`Watchable`] subscription whose
/// emissions are pushed as [`MapDiff`]s into `sink`. Tracks the latest emitted
/// `U` per source key so it can construct proper Insert/Update/Remove diffs
/// without requiring an intermediate [`CellMap`].
///
/// The runtime is generic in `U`; `project_cell`'s `Option<(K2, V2)>` projection
/// step lives in [`crate::traits::collections::project_cell`] as a follow-on
/// stage on top of this primitive.
pub fn install_map_values_cell_via_query<K, V, U, S, W, F>(
    cx: &mut crate::map_query::compiler::CompileContext,
    source: S,
    mapper: F,
    sink: crate::map_query::BoxedMapDiffSink<K, U>,
) -> Vec<SubscriptionGuard>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
    U: CellValue,
    S: crate::map_query::MapQuery<Key = K, Value = V>,
    W: Watchable<U> + Gettable<U> + Clone + Send + Sync + 'static,
    F: Fn(&K, &V) -> W + Send + Sync + 'static,
{
    let runtime = Arc::new(MapValuesRuntime::<K, V, U, W, F> {
        mapper: Arc::new(mapper),
        per_key_guards: Arc::new(DashMap::new()),
        last_value: Arc::new(Mutex::new(HashMap::new())),
        sink,
        marker: PhantomData,
    });
    let upstream_sink = move |diff: &MapDiff<K, V>| {
        if matches!(diff, MapDiff::Batch { .. }) {
            let mut atomic_changes = Vec::new();
            flatten_diff(diff, &mut atomic_changes);
            let mut downstream_changes = Vec::new();
            for change in &atomic_changes {
                runtime.apply(change, Some(&mut downstream_changes));
            }
            if !downstream_changes.is_empty() {
                (runtime.sink)(&MapDiff::Batch {
                    changes: downstream_changes,
                });
            }
        } else {
            runtime.apply(diff, None);
        }
    };

    crate::map_query::compile_runtime_into(source, cx, Arc::new(upstream_sink))
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;
    use crate::{Cell, CellMap, MapExt, Materialize, cell_map::MapDiff};

    /// Capture every emitted diff and replay it onto
    /// an in-memory `HashMap<K, U>` to assert per-row state.
    fn capturing_sink<K, U>() -> (
        crate::map_query::BoxedMapDiffSink<K, U>,
        Arc<Mutex<HashMap<K, U>>>,
        Arc<Mutex<Vec<MapDiff<K, U>>>>,
    )
    where
        K: Hash + Eq + CellValue,
        U: CellValue,
    {
        let state: Arc<Mutex<HashMap<K, U>>> = Arc::new(Mutex::new(HashMap::new()));
        let diffs: Arc<Mutex<Vec<MapDiff<K, U>>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = {
            let state = state.clone();
            let diffs = diffs.clone();
            move |diff: &MapDiff<K, U>| {
                let mut state = state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                apply_to_hashmap(&mut state, diff);
                diffs
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(diff.clone());
            }
        };
        (Arc::new(sink), state, diffs)
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
        let mut cx = crate::map_query::compiler::CompileContext::default();
        let mut guards = install_map_values_cell_via_query(
            &mut cx,
            values,
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
        guards.extend(cx.activate());
        assert!(!guards.is_empty());

        assert_eq!(
            state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&"a".to_string())
                .copied(),
            Some(10)
        );
        assert_eq!(
            state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&"b".to_string())
                .copied(),
            Some(40)
        );
        factors.insert("a".to_string(), 3);
        assert_eq!(
            state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(&"a".to_string())
                .copied(),
            Some(30)
        );
    }

    #[test]
    fn map_values_cell_preserves_upstream_batch_without_extra_emissions() {
        let source = CellMap::<String, i32>::new();

        let (tx, rx) = mpsc::channel::<MapDiff<String, i32>>();
        let sink = move |diff: &MapDiff<String, i32>| {
            let _ = tx.send(diff.clone());
        };

        let mut cx = crate::map_query::compiler::CompileContext::default();
        let mut guards = install_map_values_cell_via_query(
            &mut cx,
            source.clone(),
            |_: &String, v: &i32| Cell::new(*v * 10).lock(),
            Arc::new(sink),
        );
        guards.extend(cx.activate());
        assert!(!guards.is_empty());

        // Drain the initial empty diff emitted on subscription.
        let _ = rx.try_iter().count();

        source.insert_many(vec![("a".to_string(), 1), ("b".to_string(), 2)]);
        let seen: Vec<_> = rx.try_iter().collect();
        assert_eq!(seen.len(), 1);
        assert!(
            matches!(seen.last(), Some(MapDiff::Batch { changes }) if changes.len() == 2),
            "expected a two-change batch diff from install_map_values_cell_via_query"
        );
    }
}
