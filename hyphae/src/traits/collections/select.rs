//! Select plan node implementing [`MapQuery`].
//!
//! `select` builds an uncompiled plan node that composes with other
//! [`MapQuery`] operators. Call [`MapQuery::materialize`] to compile a plan
//! into a subscribable [`CellMap`](crate::CellMap).

use std::{hash::Hash, marker::PhantomData, sync::Arc};

use crate::{
    map_query::{MapDiffSink, MapQuery, MapQueryInstall},
    subscription::SubscriptionGuard,
    traits::{CellValue, collections::internal::map_runtime::install_map_runtime_via_query},
};

/// Plan node for [`SelectExt::select`].
///
/// Filters source rows by a predicate over the value. Output key/value
/// types match the input.
///
/// Not [`Clone`]: cloning a plan would silently duplicate filter work;
/// share by materializing once.
pub struct SelectPlan<S, K, V, F>
where
    S: MapQuery<K, V>,
    K: Hash + Eq + CellValue,
    V: CellValue,
    F: Fn(&V) -> bool + Send + Sync + 'static,
{
    pub(crate) source: S,
    pub(crate) predicate: Arc<F>,
    pub(crate) _types: PhantomData<fn() -> (K, V)>,
}

impl<S, K, V, F> MapQueryInstall<K, V> for SelectPlan<S, K, V, F>
where
    S: MapQuery<K, V>,
    K: Hash + Eq + CellValue,
    V: CellValue,
    F: Fn(&V) -> bool + Send + Sync + 'static,
{
    fn install(self, sink: MapDiffSink<K, V>) -> Vec<SubscriptionGuard> {
        let predicate = self.predicate;
        install_map_runtime_via_query::<K, V, K, V, S, _>(
            self.source,
            move |k, v| {
                if predicate(v) {
                    vec![(k.clone(), v.clone())]
                } else {
                    Vec::new()
                }
            },
            sink,
        )
    }
}

#[allow(private_bounds)]
impl<S, K, V, F> MapQuery<K, V> for SelectPlan<S, K, V, F>
where
    S: MapQuery<K, V>,
    K: Hash + Eq + CellValue,
    V: CellValue,
    F: Fn(&V) -> bool + Send + Sync + 'static,
{
}

/// Select operator returning a [`MapQuery`] plan node.
///
/// `select` consumes `self` and returns an uncompiled plan node; call
/// [`MapQuery::materialize`] on the result to obtain a subscribable
/// [`CellMap`](crate::CellMap).
pub trait SelectExt<K, V>: MapQuery<K, V>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    /// Filters rows by value.
    ///
    /// `predicate(&value)` decides whether a row is present in the output map.
    #[track_caller]
    fn select<F>(self, predicate: F) -> SelectPlan<Self, K, V, F>
    where
        F: Fn(&V) -> bool + Send + Sync + 'static,
    {
        SelectPlan {
            source: self,
            predicate: Arc::new(predicate),
            _types: PhantomData,
        }
    }
}

impl<K, V, M> SelectExt<K, V> for M
where
    K: Hash + Eq + CellValue,
    V: CellValue,
    M: MapQuery<K, V>,
{
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
        CellMap, MapDiff,
        traits::{Gettable, Watchable},
    };

    #[test]
    fn select_filters_and_updates() {
        let map = CellMap::<String, i32>::new();
        map.insert("a".to_string(), 5);
        map.insert("b".to_string(), 15);
        map.insert("c".to_string(), 25);

        let filtered = map.clone().select(|v| *v > 10).materialize();
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
        let filtered = map.clone().select(|v| *v > 10).materialize();

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
        let filtered = map.clone().select(|v| *v > 10).materialize();
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
