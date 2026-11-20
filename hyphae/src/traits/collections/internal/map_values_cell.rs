use std::{
    collections::HashMap,
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

struct MapValuesRow<U> {
    token: Arc<()>,
    value: U,
    /// Key of the row's inner subscription in `MapValuesRuntime::inner`.
    guard: Option<uuid::Uuid>,
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
    /// Locked only inside `gate`, so it is never contended and a row's
    /// replay during `attach` queues behind the event that attached it.
    rows: Mutex<HashMap<K, MapValuesRow<U>>>,
    /// A row update enters the query here. The source diff arrives inside
    /// the gate already, from its root.
    gate: crate::map_query::compiler::QueryGate,
    sink: crate::map_query::BoxedMapDiffSink<K, U>,
    /// Owns every row's inner subscription and is never written. The output
    /// depends on it, so the output sits above every inner cell and a row
    /// update joins the settle that fired the inner cell.
    inner: crate::Cell<(), crate::CellMutable>,
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
            self.release(row.guard);
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
                let key = key_for_sub.clone();
                let token = Arc::clone(&token);
                let value = (**value).clone();
                let gate = runtime.gate.clone();
                gate.run(move || runtime.row_changed(key, &token, value));
            }
        });
        if let Some(row) = rows.get_mut(key) {
            let guard_key = uuid::Uuid::new_v4();
            self.inner.own_keyed(guard_key, sub_guard);
            row.guard = Some(guard_key);
        }
    }

    fn release(&self, guard: Option<uuid::Uuid>) {
        if let Some(guard_key) = guard {
            self.inner.release_owned(guard_key);
        }
    }

    fn detach(
        &self,
        rows: &mut HashMap<K, MapValuesRow<U>>,
        key: &K,
        changes: Option<&mut Vec<MapDiff<K, U>>>,
    ) {
        if let Some(row) = rows.remove(key) {
            self.release(row.guard);
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

    fn source_changed(self: &Arc<Self>, diff: &MapDiff<K, V>) {
        let mut rows = self
            .rows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let MapDiff::Batch { .. } = diff {
            let mut atomic_changes = Vec::new();
            diff.clone().flatten_into(&mut atomic_changes);
            let mut downstream_changes = Vec::new();
            for change in &atomic_changes {
                self.apply(&mut rows, change, Some(&mut downstream_changes));
            }
            if !downstream_changes.is_empty() {
                (self.sink)(&MapDiff::Batch {
                    changes: downstream_changes,
                });
            }
        } else {
            self.apply(&mut rows, diff, None);
        }
    }

    fn row_changed(&self, key: K, token: &Arc<()>, value: U) {
        let mut rows = self
            .rows
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(row) = rows.get_mut(&key) else {
            return;
        };
        if !Arc::ptr_eq(&row.token, token) || row.value == value {
            return;
        }
        let old_value = std::mem::replace(&mut row.value, value.clone());
        drop(rows);
        (self.sink)(&MapDiff::Update {
            key,
            old_value,
            new_value: value,
        });
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
/// Install a per-entry mapper. Its node is a family node: each source diff
/// is applied in that node's fire, as a row build, so the cells the mapper
/// builds are rows. `anchors` are the outside nodes the mapper reads; the
/// node gets an edge from each, and debug builds panic on a row that reads
/// a node they do not cover. A mapper that reads only its entry passes none.
pub fn install_map_values_cell_via_query<K, V, U, S, W, F>(
    cx: &mut crate::map_query::compiler::CompileContext,
    source: S,
    mapper: F,
    sink: crate::map_query::BoxedMapDiffSink<K, U>,
    anchors: Vec<SubscriptionGuard>,
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
        rows: Mutex::new(HashMap::new()),
        gate: cx.query_gate(),
        sink,
        inner: crate::Cell::new(()),
        marker: PhantomData,
    });
    for anchor in anchors {
        runtime.inner.own(anchor);
    }
    let inner = SubscriptionGuard::dependency(&runtime.inner);
    let node: Arc<dyn crate::settle::Node> = runtime.inner.inner.clone();
    let weak = Arc::downgrade(&runtime);
    let upstream_sink = move |diff: &MapDiff<K, V>| {
        let diff = diff.clone();
        let weak = weak.clone();
        crate::settle::run_in_fire_or_now(
            &node,
            Box::new(move || {
                if let Some(runtime) = weak.upgrade() {
                    // Through the query's gate, as the root fanout ran it: a
                    // row's replay during the build queues behind this diff
                    // instead of re-entering the rows lock. The build is
                    // inside the gate, so it wraps the diff wherever the gate
                    // runs it, including after a construction-time fanout.
                    let gate = runtime.gate.clone();
                    gate.run(move || {
                        let core = std::sync::Arc::clone(&runtime.inner.inner.core);
                        crate::settle::build_rows(&core, || runtime.source_changed(&diff));
                    });
                }
            }),
        );
    };

    let start = cx.activation_count();
    let mut guards = crate::map_query::compile_runtime_into(source, cx, Arc::new(upstream_sink));
    // The diffs reach the node's fire along edges from the query's roots.
    // Roots are installed when the query activates, so each activation
    // declares its roots' edges into the node as it installs them.
    for guard in &guards {
        runtime.inner.own(SubscriptionGuard::dependency_on(guard.source()));
    }
    for activate in cx.take_activations_since(start) {
        let node = runtime.inner.clone();
        cx.push_activation(Box::new(move || {
            let installed = activate();
            for guard in &installed {
                node.own(SubscriptionGuard::dependency_on(guard.source()));
            }
            installed
        }));
    }
    guards.push(inner);
    // The runtime lives as long as the guards.
    guards.push(SubscriptionGuard::from_callback(move || {
        let _ = &runtime;
    }));
    guards
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
            // The mapper reads `factors`, so the rows anchor to it.
            vec![crate::family::FamilyInput::anchor_guard(&factors)],
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
            Vec::new(),
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
