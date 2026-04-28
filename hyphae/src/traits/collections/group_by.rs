//! Group-by plan node implementing [`MapQuery`].
//!
//! `group_by` builds an uncompiled plan node that composes with other
//! [`MapQuery`] operators. Call [`MapQuery::materialize`] to compile a plan
//! into a subscribable [`CellMap`](crate::CellMap).

use std::{collections::BTreeMap, hash::Hash, marker::PhantomData, sync::Arc};

use crate::{
    map_query::{MapDiffSink, MapQuery, MapQueryInstall},
    subscription::SubscriptionGuard,
    traits::{
        CellValue,
        collections::internal::diff_runtime::{GroupedOps, install_grouped_runtime_via_query},
    },
};

/// Plan node for [`GroupByExt::group_by`].
///
/// Groups source rows by the key produced by `group_key(&source_key,
/// &source_value)`. Each output value is the group's rows as `Vec<V>`.
///
/// Not [`Clone`]: cloning a plan would silently duplicate grouping work;
/// share by materializing once.
pub struct GroupByPlan<S, K, V, GK, F>
where
    S: MapQuery<K, V>,
    K: Hash + Eq + Ord + CellValue,
    V: CellValue,
    GK: Hash + Eq + CellValue,
    F: Fn(&K, &V) -> GK + Send + Sync + 'static,
{
    pub(crate) source: S,
    pub(crate) group_key: Arc<F>,
    pub(crate) _types: PhantomData<fn() -> (K, V, GK)>,
}

impl<S, K, V, GK, F> MapQueryInstall<GK, Vec<V>> for GroupByPlan<S, K, V, GK, F>
where
    S: MapQuery<K, V>,
    K: Hash + Eq + Ord + CellValue,
    V: CellValue,
    GK: Hash + Eq + CellValue,
    F: Fn(&K, &V) -> GK + Send + Sync + 'static,
{
    fn install(self, sink: MapDiffSink<GK, Vec<V>>) -> Vec<SubscriptionGuard> {
        let group_key = self.group_key;
        install_grouped_runtime_via_query::<K, V, GK, BTreeMap<K, V>, Vec<V>, S, _, _, _, _, _, _>(
            self.source,
            GroupedOps {
                make_group_key: move |k, v| group_key(k, v),
                on_insert: |rows: &mut BTreeMap<K, V>, key: &K, value: &V| {
                    rows.insert(key.clone(), value.clone());
                },
                on_update: |rows: &mut BTreeMap<K, V>, key: &K, _: &V, new_value: &V| {
                    rows.insert(key.clone(), new_value.clone());
                },
                on_remove: |rows: &mut BTreeMap<K, V>, key: &K, _: &V| {
                    rows.remove(key);
                },
                materialize: |rows: &BTreeMap<K, V>| rows.values().cloned().collect::<Vec<_>>(),
                is_empty: |rows: &BTreeMap<K, V>| rows.is_empty(),
                new_group_state: BTreeMap::new,
                _marker: std::marker::PhantomData,
            },
            sink,
        )
    }
}

#[allow(private_bounds)]
impl<S, K, V, GK, F> MapQuery<GK, Vec<V>> for GroupByPlan<S, K, V, GK, F>
where
    S: MapQuery<K, V>,
    K: Hash + Eq + Ord + CellValue,
    V: CellValue,
    GK: Hash + Eq + CellValue,
    F: Fn(&K, &V) -> GK + Send + Sync + 'static,
{
}

/// Group-by operator returning a [`MapQuery`] plan node.
///
/// `group_by` consumes `self` and returns an uncompiled plan node; call
/// [`MapQuery::materialize`] on the result to obtain a subscribable
/// [`CellMap`](crate::CellMap).
pub trait GroupByExt<K, V>: MapQuery<K, V>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    /// Groups source rows by `group_key(&source_key, &source_value)`.
    ///
    /// Each output key is a group id and each output value is the group's
    /// rows as `Vec<V>`.
    #[track_caller]
    fn group_by<GK, F>(self, group_key: F) -> GroupByPlan<Self, K, V, GK, F>
    where
        K: Ord,
        GK: Hash + Eq + CellValue,
        F: Fn(&K, &V) -> GK + Send + Sync + 'static,
    {
        GroupByPlan {
            source: self,
            group_key: Arc::new(group_key),
            _types: PhantomData,
        }
    }
}

impl<K, V, M> GroupByExt<K, V> for M
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
    use crate::{CellMap, MapDiff};

    #[test]
    fn group_by_preserves_upstream_batch_without_extra_emissions() {
        let source = CellMap::<String, i32>::new();
        let grouped = source.clone().group_by(|_, v| v.to_string()).materialize();

        let (tx, rx) = mpsc::channel::<MapDiff<String, Vec<i32>>>();
        let _guard = grouped.subscribe_diffs(move |diff| {
            let _ = tx.send(diff.clone());
        });

        source.insert_many(vec![("a".to_string(), 1), ("b".to_string(), 2)]);
        let seen: Vec<_> = rx.try_iter().collect();
        assert_eq!(seen.len(), 2);
        match seen.last().unwrap() {
            MapDiff::Batch { changes } => assert_eq!(changes.len(), 2),
            _ => panic!("expected batch diff from group_by"),
        }
    }
}
