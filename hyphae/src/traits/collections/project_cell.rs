//! Project-cell plan node implementing [`MapQuery`].
//!
//! `project_cell` is the reactive variant of `project`: each source row maps
//! to a [`Watchable`]`<Option<(K2, V2)>>` whose emissions update the row's
//! output. Returns an uncompiled plan node; call [`MapQuery::materialize`] to
//! compile a plan into a subscribable [`CellMap`](crate::CellMap).

use std::{
    collections::HashMap,
    hash::Hash,
    marker::PhantomData,
    sync::{Arc, Mutex},
};

use crate::{
    cell_map::MapDiff,
    map_query::{MapDiffSink, MapQuery, MapQueryInstall},
    subscription::SubscriptionGuard,
    traits::{
        CellValue, Gettable, Watchable,
        collections::internal::map_values_cell::install_map_values_cell_via_query,
    },
};

/// Plan node for [`ProjectCellExt::project_cell`].
///
/// Each source row maps to a [`Watchable`]`<Option<(K2, V2)>>`; the watchable's
/// emissions drive that row's output. `Some((k, v))` includes/updates the row;
/// `None` excludes it.
///
/// Not [`Clone`]: cloning a plan would silently duplicate per-row subscription
/// work; share by materializing once.
pub struct ProjectCellPlan<S, K, V, K2, V2, W, F>
where
    S: MapQuery<K, V>,
    K: Hash + Eq + CellValue,
    V: CellValue,
    K2: Hash + Eq + CellValue,
    V2: CellValue,
    W: Watchable<Option<(K2, V2)>> + Gettable<Option<(K2, V2)>> + Clone + Send + Sync + 'static,
    F: Fn(&K, &V) -> W + Send + Sync + 'static,
{
    pub(crate) source: S,
    pub(crate) mapper: Arc<F>,
    #[allow(clippy::type_complexity)]
    pub(crate) _types: PhantomData<fn() -> (K, V, K2, V2, W)>,
}

impl<S, K, V, K2, V2, W, F> MapQueryInstall<K2, V2> for ProjectCellPlan<S, K, V, K2, V2, W, F>
where
    S: MapQuery<K, V>,
    K: Hash + Eq + CellValue,
    V: CellValue,
    K2: Hash + Eq + CellValue,
    V2: CellValue,
    W: Watchable<Option<(K2, V2)>> + Gettable<Option<(K2, V2)>> + Clone + Send + Sync + 'static,
    F: Fn(&K, &V) -> W + Send + Sync + 'static,
{
    fn install(self, sink: MapDiffSink<K2, V2>) -> Vec<SubscriptionGuard> {
        // Stage 1 (install_map_values_cell_via_query) emits diffs keyed by `K`
        // with values `Option<(K2, V2)>`. Stage 2 — implemented inline by the
        // intermediate sink below — projects `Some(...) -> (K2, V2)` and `None
        // -> Remove`, tracking the last emitted `(K2, V2)` per source key so
        // we know what to Remove on transitions.
        let last_emitted: Arc<Mutex<HashMap<K, (K2, V2)>>> = Arc::new(Mutex::new(HashMap::new()));

        let intermediate_sink: MapDiffSink<K, Option<(K2, V2)>> = {
            let last_emitted = last_emitted.clone();
            let final_sink = sink.clone();
            Arc::new(move |diff| {
                let mut out: Vec<MapDiff<K2, V2>> = Vec::new();
                project_diff(&last_emitted, diff, &mut out);
                if out.is_empty() {
                    return;
                }
                if out.len() == 1 {
                    final_sink(&out.pop().unwrap());
                } else {
                    final_sink(&MapDiff::Batch { changes: out });
                }
            })
        };

        let mapper = self.mapper;
        install_map_values_cell_via_query::<K, V, Option<(K2, V2)>, S, W, _>(
            self.source,
            move |k, v| (mapper)(k, v),
            intermediate_sink,
        )
    }
}

#[allow(private_bounds)]
impl<S, K, V, K2, V2, W, F> MapQuery<K2, V2> for ProjectCellPlan<S, K, V, K2, V2, W, F>
where
    S: MapQuery<K, V>,
    K: Hash + Eq + CellValue,
    V: CellValue,
    K2: Hash + Eq + CellValue,
    V2: CellValue,
    W: Watchable<Option<(K2, V2)>> + Gettable<Option<(K2, V2)>> + Clone + Send + Sync + 'static,
    F: Fn(&K, &V) -> W + Send + Sync + 'static,
{
}

/// Translate a single `(K, Option<(K2, V2)>)` diff into the corresponding
/// `(K2, V2)` diffs, mutating `last_emitted` so we know what `(K2, V2)` to
/// Remove on later transitions.
///
/// Diffs from stage 1 are always atomic when they're not `Batch` (the runtime
/// emits Insert/Update/Remove/Initial). For Batch we recurse over each child.
fn project_diff<K, K2, V2>(
    last_emitted: &Arc<Mutex<HashMap<K, (K2, V2)>>>,
    diff: &MapDiff<K, Option<(K2, V2)>>,
    out: &mut Vec<MapDiff<K2, V2>>,
) where
    K: Hash + Eq + CellValue,
    K2: Hash + Eq + CellValue,
    V2: CellValue,
{
    match diff {
        MapDiff::Initial { entries } => {
            // Reset all previously emitted output entries first.
            let stale: Vec<(K2, V2)> = {
                let mut last = last_emitted.lock().unwrap_or_else(|e| e.into_inner());
                last.drain().map(|(_, v)| v).collect()
            };
            for (k2, v2) in stale {
                out.push(MapDiff::Remove {
                    key: k2,
                    old_value: v2,
                });
            }
            for (k, opt) in entries {
                apply_one(last_emitted, k, opt.clone(), out);
            }
        }
        MapDiff::Insert { key, value } => {
            apply_one(last_emitted, key, value.clone(), out);
        }
        MapDiff::Update { key, new_value, .. } => {
            apply_one(last_emitted, key, new_value.clone(), out);
        }
        MapDiff::Remove { key, .. } => {
            // Source row gone: Remove the K2 we last emitted (if any).
            let prev = {
                let mut last = last_emitted.lock().unwrap_or_else(|e| e.into_inner());
                last.remove(key)
            };
            if let Some((k2, v2)) = prev {
                out.push(MapDiff::Remove {
                    key: k2,
                    old_value: v2,
                });
            }
        }
        MapDiff::Batch { changes } => {
            for change in changes {
                project_diff(last_emitted, change, out);
            }
        }
    }
}

/// Apply a single `Option<(K2, V2)>` for source key `k`. Mutates `last_emitted`
/// and pushes resulting diffs onto `out`.
///
/// Cases (prev = `last_emitted[k]`, new = `opt`):
/// - prev = None,           new = None         : noop
/// - prev = None,           new = Some(k2, v2) : Insert(k2, v2)
/// - prev = Some(p_k2, p_v2), new = None       : Remove(p_k2)
/// - prev = Some(p_k2, p_v2), new = Some(k2, v2):
///     - p_k2 == k2 && p_v2 == v2 : noop
///     - p_k2 == k2 && p_v2 != v2 : Update(k2, p_v2 -> v2)
///     - p_k2 != k2               : Remove(p_k2) + Insert(k2, v2)
fn apply_one<K, K2, V2>(
    last_emitted: &Arc<Mutex<HashMap<K, (K2, V2)>>>,
    k: &K,
    new_opt: Option<(K2, V2)>,
    out: &mut Vec<MapDiff<K2, V2>>,
) where
    K: Hash + Eq + CellValue,
    K2: Hash + Eq + CellValue,
    V2: CellValue,
{
    let prev = {
        let mut last = last_emitted.lock().unwrap_or_else(|e| e.into_inner());
        match (&new_opt, last.get(k)) {
            (None, _) => last.remove(k),
            (Some(new_pair), _) => last.insert(k.clone(), new_pair.clone()),
        }
    };

    match (prev, new_opt) {
        (None, None) => {}
        (None, Some((k2, v2))) => {
            out.push(MapDiff::Insert { key: k2, value: v2 });
        }
        (Some((p_k2, p_v2)), None) => {
            out.push(MapDiff::Remove {
                key: p_k2,
                old_value: p_v2,
            });
        }
        (Some((p_k2, p_v2)), Some((k2, v2))) => {
            if p_k2 == k2 {
                if p_v2 != v2 {
                    out.push(MapDiff::Update {
                        key: k2,
                        old_value: p_v2,
                        new_value: v2,
                    });
                }
            } else {
                out.push(MapDiff::Remove {
                    key: p_k2,
                    old_value: p_v2,
                });
                out.push(MapDiff::Insert { key: k2, value: v2 });
            }
        }
    }
}

/// Project operator returning a [`MapQuery`] plan node.
///
/// `project_cell` builds an uncompiled plan node; call
/// [`MapQuery::materialize`] on the result to obtain a subscribable
/// [`CellMap`](crate::CellMap).
pub trait ProjectCellExt<K, V>: MapQuery<K, V>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    /// Reactive project variant: each row maps to a watchable optional output row.
    ///
    /// `mapper(&source_key, &source_value)` returns a watchable
    /// `Option<(output_key, output_value)>`. `None` means the row is excluded.
    #[track_caller]
    fn project_cell<K2, V2, W, F>(self, mapper: F) -> ProjectCellPlan<Self, K, V, K2, V2, W, F>
    where
        K2: Hash + Eq + CellValue,
        V2: CellValue,
        W: Watchable<Option<(K2, V2)>> + Gettable<Option<(K2, V2)>> + Clone + Send + Sync + 'static,
        F: Fn(&K, &V) -> W + Send + Sync + 'static,
    {
        ProjectCellPlan {
            source: self,
            mapper: Arc::new(mapper),
            _types: PhantomData,
        }
    }
}

impl<K, V, M> ProjectCellExt<K, V> for M
where
    K: Hash + Eq + CellValue,
    V: CellValue,
    M: MapQuery<K, V>,
{
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Cell, CellMap, MapExt, MaterializeDefinite};

    #[test]
    fn project_cell_reacts_to_inner_pipeline_emissions() {
        let src = CellMap::<String, i32>::new();
        let weights = CellMap::<String, i32>::new();
        weights.insert("a".to_string(), 2);
        weights.insert("b".to_string(), 3);

        src.insert("a".to_string(), 10);
        src.insert("b".to_string(), 20);

        let weights_for_mapper = weights.clone();
        let mat = src
            .clone()
            .project_cell(move |key, value| {
                let key = key.clone();
                let value = *value;
                weights_for_mapper
                    .get(&key)
                    .map(move |w| w.map(|w| (key.clone(), value * w)))
                    .materialize()
            })
            .materialize();

        assert_eq!(mat.get_value(&"a".to_string()), Some(20));
        assert_eq!(mat.get_value(&"b".to_string()), Some(60));

        // Source row updates flow through the per-row Watchable.
        src.insert("a".to_string(), 100);
        assert_eq!(mat.get_value(&"a".to_string()), Some(200));

        // Inner cell updates also flow through.
        weights.insert("a".to_string(), 5);
        assert_eq!(mat.get_value(&"a".to_string()), Some(500));
    }

    #[test]
    fn project_cell_drops_row_when_inner_emits_none() {
        let src = CellMap::<String, i32>::new();
        let gates = CellMap::<String, bool>::new();
        gates.insert("a".to_string(), true);

        src.insert("a".to_string(), 7);

        let gates_for_mapper = gates.clone();
        let mat = src
            .clone()
            .project_cell(move |key, value| {
                let key = key.clone();
                let value = *value;
                gates_for_mapper
                    .get(&key)
                    .map(move |g| {
                        if g.unwrap_or(false) {
                            Some((key.clone(), value))
                        } else {
                            None
                        }
                    })
                    .materialize()
            })
            .materialize();

        assert_eq!(mat.get_value(&"a".to_string()), Some(7));
        gates.insert("a".to_string(), false);
        assert_eq!(mat.get_value(&"a".to_string()), None);
        gates.insert("a".to_string(), true);
        assert_eq!(mat.get_value(&"a".to_string()), Some(7));
    }

    #[test]
    fn project_cell_static_inner_cell() {
        let src = CellMap::<String, i32>::new();
        src.insert("a".to_string(), 1);

        let mat = src
            .clone()
            .project_cell(|key, value| Cell::new(Some((key.clone(), *value * 10))).lock())
            .materialize();

        assert_eq!(mat.get_value(&"a".to_string()), Some(10));
        src.insert("a".to_string(), 5);
        assert_eq!(mat.get_value(&"a".to_string()), Some(50));
    }

    #[test]
    fn project_cell_remap_changes_output_key() {
        // When the inner Watchable changes the output key (e.g. Some((k, v))
        // -> Some((k', v))), we emit Remove(k) + Insert(k', v).
        let src = CellMap::<String, i32>::new();
        src.insert("a".to_string(), 1);

        let key_choice = CellMap::<String, String>::new();
        key_choice.insert("a".to_string(), "x".to_string());

        let key_choice_for_mapper = key_choice.clone();
        let mat = src
            .clone()
            .project_cell(move |k, v| {
                let v = *v;
                let k = k.clone();
                key_choice_for_mapper
                    .get(&k)
                    .map(move |kc| kc.clone().map(|kc| (kc, v)))
                    .materialize()
            })
            .materialize();

        assert_eq!(mat.get_value(&"x".to_string()), Some(1));
        assert_eq!(mat.get_value(&"y".to_string()), None);

        key_choice.insert("a".to_string(), "y".to_string());
        assert_eq!(mat.get_value(&"x".to_string()), None);
        assert_eq!(mat.get_value(&"y".to_string()), Some(1));
    }
}
