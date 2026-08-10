//! Collision-safe one-to-many projection plans implementing [`MapQuery`].

use std::{hash::Hash, marker::PhantomData};

use crate::{
    map_query::{
        MapDiffSink, MapQuery, MapQueryInstall,
        properties::{Many, PlanProperties, Repartition},
    },
    subscription::SubscriptionGuard,
    traits::{CellValue, collections::internal::map_runtime::install_map_runtime_via_query},
};

impl<S, SK, SV, LK, OV, F> PlanProperties for FlatMapEntriesPlan<S, SK, SV, LK, OV, F>
where
    S: MapQuery<Key = SK, Value = SV> + PlanProperties,
    SK: Hash + Eq + CellValue,
    SV: CellValue,
    LK: Hash + Eq + CellValue,
    OV: CellValue,
    F: Fn(&SK, &SV) -> Vec<(LK, OV)> + Send + Sync + 'static,
{
    type Cardinality = Many;
    type InputPartition = S::OutputPartition;
    type OutputPartition = Repartition<(SK, LK)>;
}

/// One-to-many projection whose `(source key, local key)` output identity is
/// collision-free across distinct source rows.
pub struct FlatMapEntriesPlan<S, SK, SV, LK, OV, F>
where
    S: MapQuery<Key = SK, Value = SV>,
    SK: Hash + Eq + CellValue,
    SV: CellValue,
    LK: Hash + Eq + CellValue,
    OV: CellValue,
    F: Fn(&SK, &SV) -> Vec<(LK, OV)> + Send + Sync + 'static,
{
    pub(crate) source: S,
    pub(crate) f: F,
    pub(crate) _types: PhantomData<fn() -> (SK, SV, LK, OV)>,
}

impl<S, SK, SV, LK, OV, F> MapQueryInstall<(SK, LK), OV>
    for FlatMapEntriesPlan<S, SK, SV, LK, OV, F>
where
    S: MapQuery<Key = SK, Value = SV>,
    SK: Hash + Eq + CellValue,
    SV: CellValue,
    LK: Hash + Eq + CellValue,
    OV: CellValue,
    F: Fn(&SK, &SV) -> Vec<(LK, OV)> + Send + Sync + 'static,
{
    fn install<Sink>(self, sink: Sink) -> Vec<SubscriptionGuard>
    where
        Sink: MapDiffSink<(SK, LK), OV>,
    {
        let f = self.f;
        install_map_runtime_via_query(
            self.source,
            move |source_key, value| {
                f(source_key, value)
                    .into_iter()
                    .map(|(local_key, output)| ((source_key.clone(), local_key), output))
                    .collect()
            },
            sink,
        )
    }
}

#[allow(private_bounds)]
impl<S, SK, SV, LK, OV, F> MapQuery for FlatMapEntriesPlan<S, SK, SV, LK, OV, F>
where
    S: MapQuery<Key = SK, Value = SV>,
    SK: Hash + Eq + CellValue,
    SV: CellValue,
    LK: Hash + Eq + CellValue,
    OV: CellValue,
    F: Fn(&SK, &SV) -> Vec<(LK, OV)> + Send + Sync + 'static,
{
    type Key = (SK, LK);
    type Value = OV;
}

/// Semantic one-to-many projection operator.
pub trait FlatMapEntriesExt<K, V>: MapQuery<Key = K, Value = V>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    /// Expand each source row into locally keyed rows. The output identity is
    /// `(source_key, local_key)`, preventing collisions between source rows.
    fn flat_map_entries<LK, V2, F>(self, f: F) -> impl MapQuery<Key = (K, LK), Value = V2>
    where
        LK: Hash + Eq + CellValue,
        V2: CellValue,
        F: Fn(&K, &V) -> Vec<(LK, V2)> + Send + Sync + 'static,
    {
        FlatMapEntriesPlan {
            source: self,
            f,
            _types: PhantomData,
        }
    }
}

impl<K, V, M> FlatMapEntriesExt<K, V> for M
where
    K: Hash + Eq + CellValue,
    V: CellValue,
    M: MapQuery<Key = K, Value = V>,
{
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CellMap;

    #[test]
    fn flat_map_entries_emits_multiple_rows_per_source() {
        let source = CellMap::<String, i32>::new();
        let out = source
            .clone()
            .flat_map_entries(|_, value| {
                if *value <= 0 {
                    return Vec::new();
                }
                vec![
                    ("a".to_string(), value * 10),
                    ("b".to_string(), value * 100),
                ]
            })
            .materialize();

        source.insert("x".to_string(), 2);
        assert_eq!(out.get_value(&("x".to_string(), "a".to_string())), Some(20));
        assert_eq!(
            out.get_value(&("x".to_string(), "b".to_string())),
            Some(200)
        );

        source.insert("x".to_string(), 0);
        assert_eq!(out.get_value(&("x".to_string(), "a".to_string())), None);
        assert_eq!(out.get_value(&("x".to_string(), "b".to_string())), None);
    }

    #[test]
    fn flat_map_entries_scopes_local_keys_by_source() {
        let source = CellMap::<String, i32>::new();
        let out = source
            .clone()
            .flat_map_entries(|_key, value| vec![("same", value * 10)])
            .materialize();

        source.insert("a".to_string(), 1);
        source.insert("b".to_string(), 2);

        assert_eq!(out.get_value(&("a".to_string(), "same")), Some(10));
        assert_eq!(out.get_value(&("b".to_string(), "same")), Some(20));
    }
}
