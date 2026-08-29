//! Rekeying projection plan nodes implementing [`MapQuery`].

use std::{hash::Hash, marker::PhantomData};

use crate::{
    map_query::{
        BuildQueryRuntime, MapQuery,
        properties::{ExactlyOne, PlanProperties, Repartition, ZeroOrOne},
    },
    subscription::SubscriptionGuard,
    traits::{CellValue, collections::internal::map_runtime::install_map_runtime_via_query},
};

impl<S, SK, SV, OK, OV, F> PlanProperties for ProjectPlan<S, SK, SV, OK, OV, F>
where
    S: MapQuery<Key = SK, Value = SV> + PlanProperties,
    SK: Hash + Eq + CellValue,
    SV: CellValue,
    OK: Hash + Eq + CellValue,
    OV: CellValue,
    F: Fn(&SK, &SV) -> Option<(OK, OV)> + Send + Sync + 'static,
{
    type Cardinality = ZeroOrOne;
    type InputPartition = S::OutputPartition;
    type OutputPartition = Repartition<OK>;
}

impl<S, SK, SV, OK, OV, F> PlanProperties for MapEntriesPlan<S, SK, SV, OK, OV, F>
where
    S: MapQuery<Key = SK, Value = SV> + PlanProperties,
    SK: Hash + Eq + CellValue,
    SV: CellValue,
    OK: Hash + Eq + CellValue,
    OV: CellValue,
    F: Fn(&SK, &SV) -> (OK, OV) + Send + Sync + 'static,
{
    type Cardinality = ExactlyOne;
    type InputPartition = S::OutputPartition;
    type OutputPartition = Repartition<OK>;
}

/// Zero-or-one rekeying projection plan.
///
/// Each source row maps to at most one output row. The closure returns
/// `Some((output_key, output_value))` to include/update a row, or `None` to
/// exclude that source row from the output.
///
/// Not [`Clone`]: cloning a plan would silently duplicate projection work;
/// share by materializing once.
pub struct ProjectPlan<S, SK, SV, OK, OV, F>
where
    S: MapQuery<Key = SK, Value = SV>,
    SK: Hash + Eq + CellValue,
    SV: CellValue,
    OK: Hash + Eq + CellValue,
    OV: CellValue,
    F: Fn(&SK, &SV) -> Option<(OK, OV)> + Send + Sync + 'static,
{
    pub(crate) source: S,
    pub(crate) f: F,
    #[allow(clippy::type_complexity)]
    pub(crate) _types: PhantomData<fn() -> (SK, SV, OK, OV)>,
}

impl<S, SK, SV, OK, OV, F> BuildQueryRuntime<OK, OV> for ProjectPlan<S, SK, SV, OK, OV, F>
where
    S: MapQuery<Key = SK, Value = SV>,
    SK: Hash + Eq + CellValue,
    SV: CellValue,
    OK: Hash + Eq + CellValue,
    OV: CellValue,
    F: Fn(&SK, &SV) -> Option<(OK, OV)> + Send + Sync + 'static,
{
    fn build_into(
        self,
        cx: &mut crate::map_query::compiler::CompileContext,
        sink: crate::map_query::BoxedMapDiffSink<OK, OV>,
    ) -> Vec<SubscriptionGuard> {
        let f = self.f;
        install_map_runtime_via_query::<SK, SV, OK, OV, S, _>(
            cx,
            self.source,
            move |k, v| f(k, v).into_iter().collect(),
            sink,
        )
    }
}

#[allow(private_bounds)]
impl<S, SK, SV, OK, OV, F> MapQuery for ProjectPlan<S, SK, SV, OK, OV, F>
where
    S: MapQuery<Key = SK, Value = SV>,
    SK: Hash + Eq + CellValue,
    SV: CellValue,
    OK: Hash + Eq + CellValue,
    OV: CellValue,
    F: Fn(&SK, &SV) -> Option<(OK, OV)> + Send + Sync + 'static,
{
    type Key = OK;
    type Value = OV;
}

/// One-to-one rekeying projection plan.
pub struct MapEntriesPlan<S, SK, SV, OK, OV, F>
where
    S: MapQuery<Key = SK, Value = SV>,
    SK: Hash + Eq + CellValue,
    SV: CellValue,
    OK: Hash + Eq + CellValue,
    OV: CellValue,
    F: Fn(&SK, &SV) -> (OK, OV) + Send + Sync + 'static,
{
    pub(crate) source: S,
    pub(crate) f: F,
    pub(crate) _types: PhantomData<fn() -> (SK, SV, OK, OV)>,
}

impl<S, SK, SV, OK, OV, F> BuildQueryRuntime<OK, OV> for MapEntriesPlan<S, SK, SV, OK, OV, F>
where
    S: MapQuery<Key = SK, Value = SV>,
    SK: Hash + Eq + CellValue,
    SV: CellValue,
    OK: Hash + Eq + CellValue,
    OV: CellValue,
    F: Fn(&SK, &SV) -> (OK, OV) + Send + Sync + 'static,
{
    fn build_into(
        self,
        cx: &mut crate::map_query::compiler::CompileContext,
        sink: crate::map_query::BoxedMapDiffSink<OK, OV>,
    ) -> Vec<SubscriptionGuard> {
        let f = self.f;
        install_map_runtime_via_query(cx, self.source, move |k, v| vec![f(k, v)], sink)
    }
}

#[allow(private_bounds)]
impl<S, SK, SV, OK, OV, F> MapQuery for MapEntriesPlan<S, SK, SV, OK, OV, F>
where
    S: MapQuery<Key = SK, Value = SV>,
    SK: Hash + Eq + CellValue,
    SV: CellValue,
    OK: Hash + Eq + CellValue,
    OV: CellValue,
    F: Fn(&SK, &SV) -> (OK, OV) + Send + Sync + 'static,
{
    type Key = OK;
    type Value = OV;
}

/// Semantic rekeying projections. Prefer key-preserving `map_values` when the
/// key does not change; these methods explicitly create a repartition boundary.
pub trait MapEntriesExt<K, V>: MapQuery<Key = K, Value = V>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    /// Map every input row to exactly one output row with a unique output key.
    ///
    /// This is a one-to-one rekeying projection, not a many-to-one reduction.
    /// The source-to-output key mapping must be injective: two current source
    /// rows must never return the same output key, even when their output values
    /// are equal. Use [`GroupByExt::group_by`](super::GroupByExt::group_by) when
    /// several source rows intentionally share an output key.
    ///
    /// A collision is validated before output mutation and panics synchronously;
    /// it never overwrites a prior owner. This semantic rekey is a repartition
    /// boundary. The closure follows [`MapQuery`]'s purity/invocation contract.
    fn map_entries<K2, V2, F>(self, f: F) -> impl MapQuery<Key = K2, Value = V2>
    where
        K2: Hash + Eq + CellValue,
        V2: CellValue,
        F: Fn(&K, &V) -> (K2, V2) + Send + Sync + 'static,
    {
        MapEntriesPlan {
            source: self,
            f,
            _types: PhantomData,
        }
    }

    /// Map an input row to zero or one output row with a unique output key.
    ///
    /// This is a filtering rekeying projection, not a many-to-one reduction.
    /// Included rows obey the same injective key mapping, pre-mutation
    /// validation, and synchronous-panic contract as [`Self::map_entries`]. Use
    /// [`GroupByExt::group_by`](super::GroupByExt::group_by) when several
    /// included source rows intentionally share an output key. The closure
    /// follows [`MapQuery`]'s purity/invocation contract.
    fn filter_map_entries<K2, V2, F>(self, f: F) -> impl MapQuery<Key = K2, Value = V2>
    where
        K2: Hash + Eq + CellValue,
        V2: CellValue,
        F: Fn(&K, &V) -> Option<(K2, V2)> + Send + Sync + 'static,
    {
        ProjectPlan {
            source: self,
            f,
            _types: PhantomData,
        }
    }
}

impl<K, V, M> MapEntriesExt<K, V> for M
where
    K: Hash + Eq + CellValue,
    V: CellValue,
    M: MapQuery<Key = K, Value = V>,
{
}

#[cfg(test)]
mod tests {
    use std::panic::{AssertUnwindSafe, catch_unwind};

    use super::*;
    use crate::CellMap;

    #[test]
    fn project_handles_projection_disappearance_without_extra_rows() {
        let source = CellMap::<String, i32>::new();
        let projected = source
            .clone()
            .filter_map_entries(|key, value| {
                if *value > 0 {
                    Some((format!("p:{key}"), value * 10))
                } else {
                    None
                }
            })
            .materialize();

        source.insert("a".to_string(), 1);
        assert_eq!(projected.get_value(&"p:a".to_string()), Some(10));
        source.insert("a".to_string(), -1);
        assert_eq!(projected.get_value(&"p:a".to_string()), None);
    }

    #[test]
    fn semantic_entry_projections_rekey_and_filter() {
        let source = CellMap::<String, i32>::new();
        let mapped = source
            .clone()
            .map_entries(|key, value| (format!("mapped:{key}"), value * 2))
            .materialize();
        let filtered = source
            .clone()
            .filter_map_entries(|key, value| {
                (*value > 0).then(|| (format!("positive:{key}"), value * 3))
            })
            .materialize();

        source.insert("a".to_string(), 2);
        assert_eq!(mapped.get_value(&"mapped:a".to_string()), Some(4));
        assert_eq!(filtered.get_value(&"positive:a".to_string()), Some(6));

        source.insert("a".to_string(), -1);
        assert_eq!(mapped.get_value(&"mapped:a".to_string()), Some(-2));
        assert_eq!(filtered.get_value(&"positive:a".to_string()), None);
    }

    #[test]
    fn map_entries_rejects_duplicate_output_keys() {
        let source = CellMap::<u64, u64>::new();
        source.insert_many(vec![(1, 10), (2, 20)]);

        let result = catch_unwind(AssertUnwindSafe(|| {
            source.map_entries(|_, value| (0, *value)).materialize()
        }));

        assert!(result.is_err());
    }

    #[test]
    fn map_entries_allows_atomic_output_key_swaps() {
        let source = CellMap::<u64, u64>::new();
        source.insert_many(vec![(1, 2), (2, 1)]);
        let output = source
            .clone()
            .map_entries(|_, value| (*value, *value))
            .materialize();

        source.insert_many(vec![(1, 1), (2, 2)]);

        assert_eq!(output.get_value(&1), Some(1));
        assert_eq!(output.get_value(&2), Some(2));
    }
}
