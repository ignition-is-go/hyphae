//! Count-by plan node implementing [`MapQuery`].
//!
//! `count_by` builds an uncompiled plan node that composes with other
//! [`MapQuery`] operators. Call [`MapQuery::materialize`] to compile a plan
//! into a subscribable [`CellMap`](crate::CellMap).

use std::{hash::Hash, marker::PhantomData, sync::Arc};

use crate::{
    map_query::{
        BuildQueryRuntime, MapQuery,
        properties::{Many, PlanProperties, Repartition},
    },
    subscription::SubscriptionGuard,
    traits::{
        CellValue,
        collections::internal::diff_runtime::{GroupedOps, install_grouped_runtime_via_query},
    },
};

impl<S, K, V, GK, F> PlanProperties for CountByPlan<S, K, V, GK, F>
where
    S: MapQuery<Key = K, Value = V>,
    K: Hash + Eq + CellValue,
    V: CellValue,
    GK: Hash + Eq + CellValue,
    F: Fn(&K, &V) -> GK + Send + Sync + 'static,
{
    type Cardinality = Many;
    type InputPartition = S::OutputPartition;
    type OutputPartition = Repartition<GK>;
}

/// Plan node for [`CountByExt::count_by`].
///
/// Groups source rows by the key produced by `group_key(&source_key,
/// &source_value)` and stores how many rows are in each group.
///
/// Not [`Clone`]: cloning a plan would silently duplicate grouping work;
/// share by materializing once.
pub struct CountByPlan<S, K, V, GK, F>
where
    S: MapQuery<Key = K, Value = V>,
    K: Hash + Eq + CellValue,
    V: CellValue,
    GK: Hash + Eq + CellValue,
    F: Fn(&K, &V) -> GK + Send + Sync + 'static,
{
    pub(crate) source: S,
    pub(crate) group_key: Arc<F>,
    pub(crate) _types: PhantomData<fn() -> (K, V, GK)>,
}

impl<S, K, V, GK, F> BuildQueryRuntime<GK, usize> for CountByPlan<S, K, V, GK, F>
where
    S: MapQuery<Key = K, Value = V>,
    K: Hash + Eq + CellValue,
    V: CellValue,
    GK: Hash + Eq + CellValue,
    F: Fn(&K, &V) -> GK + Send + Sync + 'static,
{
    fn build_into(
        self,
        cx: &mut crate::map_query::compiler::CompileContext,
        sink: crate::map_query::BoxedMapDiffSink<GK, usize>,
    ) -> Vec<SubscriptionGuard> {
        let group_key = self.group_key;
        install_grouped_runtime_via_query::<K, V, GK, usize, usize, S, _, _, _, _, _, _>(
            cx,
            self.source,
            GroupedOps {
                make_group_key: move |k, v| group_key(k, v),
                on_insert: |group_count: &mut usize, _: &K, _: &V| {
                    *group_count = group_count.saturating_add(1);
                },
                on_update: |_: &mut usize, _: &K, _: &V, _: &V| {},
                on_remove: |group_count: &mut usize, _: &K, _: &V| {
                    *group_count = group_count.saturating_sub(1);
                },
                materialize: |group_count: &usize| *group_count,
                is_empty: |group_count: &usize| *group_count == 0,
                new_group_state: || 0usize,
                _marker: std::marker::PhantomData,
            },
            sink,
        )
    }
}

#[allow(private_bounds)]
impl<S, K, V, GK, F> MapQuery for CountByPlan<S, K, V, GK, F>
where
    S: MapQuery<Key = K, Value = V>,
    K: Hash + Eq + CellValue,
    V: CellValue,
    GK: Hash + Eq + CellValue,
    F: Fn(&K, &V) -> GK + Send + Sync + 'static,
{
    type Key = GK;
    type Value = usize;
}

/// Count-by operator returning a [`MapQuery`] plan node.
///
/// `count_by` consumes `self` and returns an uncompiled plan node; call
/// [`MapQuery::materialize`] on the result to obtain a subscribable
/// [`CellMap`](crate::CellMap).
pub trait CountByExt<K, V>: MapQuery<Key = K, Value = V>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    /// Groups rows by `group_key(&source_key, &source_value)` and stores how
    /// many rows are in each group.
    ///
    /// `group_key` is called for every source row and its return value
    /// becomes the output map key.
    #[track_caller]
    fn count_by<GK, F>(self, group_key: F) -> impl MapQuery<Key = GK, Value = usize>
    where
        GK: Hash + Eq + CellValue,
        F: Fn(&K, &V) -> GK + Send + Sync + 'static,
    {
        CountByPlan {
            source: self,
            group_key: Arc::new(group_key),
            _types: PhantomData,
        }
    }
}

impl<K, V, M> CountByExt<K, V> for M
where
    K: Hash + Eq + CellValue,
    V: CellValue,
    M: MapQuery<Key = K, Value = V>,
{
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;
    use crate::{CellMap, MapDiff};

    #[test]
    fn count_by_preserves_upstream_batch_without_extra_emissions() {
        let source = CellMap::<String, i32>::new();
        let counts = source.clone().count_by(|_, v| v.to_string()).materialize();

        let (tx, rx) = mpsc::channel::<MapDiff<String, usize>>();
        let _guard = counts.subscribe_diffs(move |diff| {
            let _ = tx.send(diff.clone());
        });

        source.insert_many(vec![("a".to_string(), 1), ("b".to_string(), 2)]);
        let seen: Vec<_> = rx.try_iter().collect();
        assert_eq!(seen.len(), 2);
        assert!(matches!(
            seen.last(),
            Some(MapDiff::Batch { changes }) if changes.len() == 2
        ));
    }
}
