use std::{
    collections::{HashMap, VecDeque},
    hash::Hash,
    marker::PhantomData,
    sync::{Arc, Mutex},
};

use crate::{
    cell_map::MapDiff,
    signal::Signal,
    subscription::SubscriptionGuard,
    traits::{CellValue, Gettable, Watchable},
};

#[cfg(test)]
#[path = "map_values_cell_initialization_tests.rs"]
mod initialization_tests;

enum MapValuesEvent<K, V, U> {
    Source(MapDiff<K, V>),
    Inner { key: K, token: Arc<()>, value: U },
}

struct MapValuesRow<U> {
    token: Arc<()>,
    value: U,
    guard: Option<SubscriptionGuard>,
}

struct MapValuesQueue<K, V, U> {
    events: VecDeque<MapValuesEvent<K, V, U>>,
    idle_rows: Option<HashMap<K, MapValuesRow<U>>>,
}

struct MapValuesRuntime<K, V, U, W, F>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
    U: CellValue,
    W: Watchable<U> + Gettable<U> + Clone + Send + Sync + 'static,
    F: Fn(&K, &V) -> W + Send + Sync + 'static,
{
    mapper: Arc<F>,
    queue: Mutex<MapValuesQueue<K, V, U>>,
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

    fn attach(
        self: &Arc<Self>,
        rows: &mut HashMap<K, MapValuesRow<U>>,
        key: &K,
        value: &V,
        changes: Option<&mut Vec<MapDiff<K, U>>>,
    ) {
        let prior = rows.remove(key).map(|row| {
            drop(row.guard);
            row.value
        });
        let inner_cell = (self.mapper)(key, value);
        let initial = inner_cell.get();
        let token = Arc::new(());
        rows.insert(
            key.clone(),
            MapValuesRow {
                token: Arc::clone(&token),
                value: initial.clone(),
                guard: None,
            },
        );
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
        let runtime = Arc::downgrade(self);
        let sub_guard = inner_cell.subscribe(move |signal| {
            if let Signal::Value(value) = signal
                && let Some(runtime) = runtime.upgrade()
            {
                runtime.enqueue(MapValuesEvent::Inner {
                    key: key_for_sub.clone(),
                    token: Arc::clone(&token),
                    value: (**value).clone(),
                });
            }
        });
        if let Some(row) = rows.get_mut(key) {
            row.guard = Some(sub_guard);
        }
    }

    fn detach(
        &self,
        rows: &mut HashMap<K, MapValuesRow<U>>,
        key: &K,
        changes: Option<&mut Vec<MapDiff<K, U>>>,
    ) {
        if let Some(row) = rows.remove(key) {
            drop(row.guard);
            self.emit(
                MapDiff::Remove {
                    key: key.clone(),
                    old_value: row.value,
                },
                changes,
            );
        }
    }

    fn apply(
        self: &Arc<Self>,
        rows: &mut HashMap<K, MapValuesRow<U>>,
        diff: &MapDiff<K, V>,
        mut changes: Option<&mut Vec<MapDiff<K, U>>>,
    ) {
        match diff {
            MapDiff::Initial { entries } => {
                let existing_keys: Vec<K> = rows.keys().cloned().collect();
                for key in existing_keys {
                    self.detach(rows, &key, changes.as_deref_mut());
                }
                for (key, value) in entries {
                    self.attach(rows, key, value, changes.as_deref_mut());
                }
            }
            MapDiff::Insert { key, value } => {
                self.attach(rows, key, value, changes);
            }
            MapDiff::Remove { key, .. } => self.detach(rows, key, changes),
            MapDiff::Update { key, new_value, .. } => {
                self.attach(rows, key, new_value, changes);
            }
            MapDiff::Batch { .. } => {}
        }
    }

    fn enqueue(self: &Arc<Self>, event: MapValuesEvent<K, V, U>) {
        let mut rows = {
            let mut queue = self
                .queue
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            queue.events.push_back(event);
            let Some(rows) = queue.idle_rows.take() else {
                return;
            };
            drop(queue);
            rows
        };
        loop {
            let event = {
                let mut queue = self
                    .queue
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let Some(event) = queue.events.pop_front() else {
                    queue.idle_rows = Some(rows);
                    return;
                };
                drop(queue);
                event
            };
            match event {
                MapValuesEvent::Source(MapDiff::Batch { changes }) => {
                    let mut atomic_changes = Vec::new();
                    MapDiff::Batch { changes }.flatten_into(&mut atomic_changes);
                    let mut downstream_changes = Vec::new();
                    for change in &atomic_changes {
                        self.apply(&mut rows, change, Some(&mut downstream_changes));
                    }
                    if !downstream_changes.is_empty() {
                        (self.sink)(&MapDiff::Batch {
                            changes: downstream_changes,
                        });
                    }
                }
                MapValuesEvent::Source(diff) => self.apply(&mut rows, &diff, None),
                MapValuesEvent::Inner { key, token, value } => {
                    let Some(row) = rows.get_mut(&key) else {
                        continue;
                    };
                    if !Arc::ptr_eq(&row.token, &token) || row.value == value {
                        continue;
                    }
                    let old_value = std::mem::replace(&mut row.value, value.clone());
                    (self.sink)(&MapDiff::Update {
                        key,
                        old_value,
                        new_value: value,
                    });
                }
            }
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
        queue: Mutex::new(MapValuesQueue {
            events: VecDeque::new(),
            idle_rows: Some(HashMap::new()),
        }),
        sink,
        marker: PhantomData,
    });
    let upstream_sink = move |diff: &MapDiff<K, V>| {
        runtime.enqueue(MapValuesEvent::Source(diff.clone()));
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
