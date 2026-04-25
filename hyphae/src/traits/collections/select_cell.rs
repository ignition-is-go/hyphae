//! Select-cell plan node implementing [`MapQuery`].
//!
//! `select_cell` is the reactive variant of `select`: each row's inclusion is
//! gated by a [`Watchable`]`<bool>`. Returns an uncompiled plan node; call
//! [`MapQuery::materialize`] to compile a plan into a subscribable
//! [`CellMap`](crate::CellMap).

use std::{hash::Hash, marker::PhantomData, sync::Arc};

use super::ProjectCellExt;
use crate::{
    map_query::{MapDiffSink, MapQuery, MapQueryInstall},
    pipeline::Pipeline,
    subscription::SubscriptionGuard,
    traits::{CellValue, Gettable, MapExt, collections::ProjectCellPlan},
};

/// Plan node for [`SelectCellExt::select_cell`].
///
/// Reactive filter: each row's inclusion is decided by a per-row
/// [`Watchable`]`<bool>` produced by `predicate(&key, &value)`. The row is
/// included while the gate is `true` and excluded when `false`.
///
/// Not [`Clone`]: cloning a plan would silently duplicate per-row
/// subscription work; share by materializing once.
pub struct SelectCellPlan<S, K, V, W, F>
where
    S: MapQuery<K, V>,
    K: Hash + Eq + CellValue,
    V: CellValue,
    W: Pipeline<bool> + Gettable<bool> + Clone + Send + Sync + 'static,
    F: Fn(&K, &V) -> W + Send + Sync + 'static,
{
    pub(crate) source: S,
    pub(crate) predicate: Arc<F>,
    pub(crate) _types: PhantomData<fn() -> (K, V, W)>,
}

impl<S, K, V, W, F> MapQueryInstall<K, V> for SelectCellPlan<S, K, V, W, F>
where
    S: MapQuery<K, V>,
    K: Hash + Eq + CellValue,
    V: CellValue,
    W: Pipeline<bool> + Gettable<bool> + Clone + Send + Sync + 'static,
    F: Fn(&K, &V) -> W + Send + Sync + 'static,
{
    fn install(self, sink: MapDiffSink<K, V>) -> Vec<SubscriptionGuard> {
        // Implement select_cell as a project_cell whose inner Watchable maps
        // the boolean gate into Option<(K, V)>.
        let predicate = self.predicate;
        let inner_plan: ProjectCellPlan<S, K, V, K, V, _, _> =
            self.source.project_cell(move |k, v| {
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
            });
        inner_plan.install(sink)
    }
}

#[allow(private_bounds)]
impl<S, K, V, W, F> MapQuery<K, V> for SelectCellPlan<S, K, V, W, F>
where
    S: MapQuery<K, V>,
    K: Hash + Eq + CellValue,
    V: CellValue,
    W: Pipeline<bool> + Gettable<bool> + Clone + Send + Sync + 'static,
    F: Fn(&K, &V) -> W + Send + Sync + 'static,
{
}

/// Select-cell operator returning a [`MapQuery`] plan node.
///
/// `select_cell` consumes `self` and returns an uncompiled plan node; call
/// [`MapQuery::materialize`] on the result to obtain a subscribable
/// [`CellMap`](crate::CellMap).
pub trait SelectCellExt<K, V>: MapQuery<K, V>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    /// Reactive filter: each row has a watchable boolean gate.
    ///
    /// `predicate(&key, &value)` returns a watchable bool; the row is
    /// included when true and excluded when false.
    #[track_caller]
    fn select_cell<W, F>(self, predicate: F) -> SelectCellPlan<Self, K, V, W, F>
    where
        W: Pipeline<bool> + Gettable<bool> + Clone + Send + Sync + 'static,
        F: Fn(&K, &V) -> W + Send + Sync + 'static,
    {
        SelectCellPlan {
            source: self,
            predicate: Arc::new(predicate),
            _types: PhantomData,
        }
    }
}

impl<K, V, M> SelectCellExt<K, V> for M
where
    K: Hash + Eq + CellValue,
    V: CellValue,
    M: MapQuery<K, V>,
{
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;
    use crate::{Cell, CellMap, MapExt, cell_map::MapDiff, pipeline::Pipeline};

    #[test]
    fn select_cell_reacts_to_predicate_changes() {
        let values = CellMap::<String, i32>::new();
        let gates = CellMap::<String, bool>::new();

        values.insert("a".to_string(), 10);
        values.insert("b".to_string(), 20);
        gates.insert("a".to_string(), false);
        gates.insert("b".to_string(), true);

        let filtered = values
            .clone()
            .select_cell({
                let gates = gates.clone();
                move |key, _value| gates.get(key).map(|v| v.unwrap_or(false)).materialize()
            })
            .materialize();

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
        let out = source
            .clone()
            .select_cell(|_, _| Cell::new(true).lock())
            .materialize();

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
