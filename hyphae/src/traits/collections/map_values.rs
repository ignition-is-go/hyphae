//! Key-preserving projection plan nodes.

use std::{hash::Hash, marker::PhantomData};

use crate::{
    map_query::{MapDiffSink, MapQuery, MapQueryInstall},
    subscription::SubscriptionGuard,
    traits::{
        CellValue,
        collections::internal::stateless_runtime::{
            install_filter_map_values_runtime, install_map_values_runtime,
        },
    },
};

/// Exactly-one, key-preserving value projection.
pub struct MapValuesPlan<S, K, V, U, F>
where
    S: MapQuery<Key = K, Value = V>,
    K: Hash + Eq + CellValue,
    V: CellValue,
    U: CellValue,
    F: Fn(&K, &V) -> U + Send + Sync + 'static,
{
    pub(crate) source: S,
    pub(crate) f: F,
    _types: PhantomData<fn() -> (K, V, U)>,
}

impl<S, K, V, U, F> MapQueryInstall<K, U> for MapValuesPlan<S, K, V, U, F>
where
    S: MapQuery<Key = K, Value = V>,
    K: Hash + Eq + CellValue,
    V: CellValue,
    U: CellValue,
    F: Fn(&K, &V) -> U + Send + Sync + 'static,
{
    fn install<Sink>(self, sink: Sink) -> Vec<SubscriptionGuard>
    where
        Sink: MapDiffSink<K, U>,
    {
        install_map_values_runtime(self.source, self.f, sink)
    }
}

#[allow(private_bounds)]
impl<S, K, V, U, F> MapQuery for MapValuesPlan<S, K, V, U, F>
where
    S: MapQuery<Key = K, Value = V>,
    K: Hash + Eq + CellValue,
    V: CellValue,
    U: CellValue,
    F: Fn(&K, &V) -> U + Send + Sync + 'static,
{
    type Key = K;
    type Value = U;
}

/// Zero-or-one, key-preserving value projection.
pub struct FilterMapValuesPlan<S, K, V, U, F>
where
    S: MapQuery<Key = K, Value = V>,
    K: Hash + Eq + CellValue,
    V: CellValue,
    U: CellValue,
    F: Fn(&K, &V) -> Option<U> + Send + Sync + 'static,
{
    source: S,
    f: F,
    _types: PhantomData<fn() -> (K, V, U)>,
}

impl<S, K, V, U, F> MapQueryInstall<K, U> for FilterMapValuesPlan<S, K, V, U, F>
where
    S: MapQuery<Key = K, Value = V>,
    K: Hash + Eq + CellValue,
    V: CellValue,
    U: CellValue,
    F: Fn(&K, &V) -> Option<U> + Send + Sync + 'static,
{
    fn install<Sink>(self, sink: Sink) -> Vec<SubscriptionGuard>
    where
        Sink: MapDiffSink<K, U>,
    {
        install_filter_map_values_runtime(self.source, self.f, sink)
    }
}

#[allow(private_bounds)]
impl<S, K, V, U, F> MapQuery for FilterMapValuesPlan<S, K, V, U, F>
where
    S: MapQuery<Key = K, Value = V>,
    K: Hash + Eq + CellValue,
    V: CellValue,
    U: CellValue,
    F: Fn(&K, &V) -> Option<U> + Send + Sync + 'static,
{
    type Key = K;
    type Value = U;
}

/// Semantic key-preserving projection operators.
pub trait MapValuesExt<K, V>: MapQuery<Key = K, Value = V>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    fn map_values<U, F>(self, f: F) -> MapValuesPlan<Self, K, V, U, F>
    where
        U: CellValue,
        F: Fn(&K, &V) -> U + Send + Sync + 'static,
    {
        MapValuesPlan {
            source: self,
            f,
            _types: PhantomData,
        }
    }

    fn filter_map_values<U, F>(self, f: F) -> FilterMapValuesPlan<Self, K, V, U, F>
    where
        U: CellValue,
        F: Fn(&K, &V) -> Option<U> + Send + Sync + 'static,
    {
        FilterMapValuesPlan {
            source: self,
            f,
            _types: PhantomData,
        }
    }
}

impl<K, V, M> MapValuesExt<K, V> for M
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
    fn key_preserving_projections_track_membership_and_values() {
        let source = CellMap::<u64, u64>::new();
        source.insert(1, 5);
        source.insert(2, 10);

        let output = source
            .clone()
            .filter_map_values(|_key, value| (*value >= 10).then_some(value * 2))
            .map_values(|_key, value| value + 1)
            .materialize();

        assert_eq!(output.get_value(&1), None);
        assert_eq!(output.get_value(&2), Some(21));

        source.insert(1, 12);
        source.insert(2, 3);
        assert_eq!(output.get_value(&1), Some(25));
        assert_eq!(output.get_value(&2), None);
    }
}
