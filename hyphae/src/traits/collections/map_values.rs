//! Key-preserving projection plan nodes.

use std::{hash::Hash, marker::PhantomData};

use crate::{
    map_query::{
        BuildQueryRuntime, MapQuery,
        properties::{ExactlyOne, PlanProperties, ZeroOrOne},
    },
    subscription::SubscriptionGuard,
    traits::{
        CellValue,
        collections::internal::stateless_runtime::{
            install_filter_map_values_runtime, install_map_values_runtime,
        },
    },
};

impl<S, K, V, U, F> PlanProperties for MapValuesPlan<S, K, V, U, F>
where
    S: MapQuery<Key = K, Value = V> + PlanProperties,
    K: Hash + Eq + CellValue,
    V: CellValue,
    U: CellValue,
    F: Fn(&K, &V) -> U + Send + Sync + 'static,
{
    type Cardinality = ExactlyOne;
    type InputPartition = S::OutputPartition;
    type OutputPartition = S::OutputPartition;
}

impl<S, K, V, U, F> PlanProperties for FilterMapValuesPlan<S, K, V, U, F>
where
    S: MapQuery<Key = K, Value = V> + PlanProperties,
    K: Hash + Eq + CellValue,
    V: CellValue,
    U: CellValue,
    F: Fn(&K, &V) -> Option<U> + Send + Sync + 'static,
{
    type Cardinality = ZeroOrOne;
    type InputPartition = S::OutputPartition;
    type OutputPartition = S::OutputPartition;
}

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

impl<S, K, V, U, F> BuildQueryRuntime<K, U> for MapValuesPlan<S, K, V, U, F>
where
    S: MapQuery<Key = K, Value = V>,
    K: Hash + Eq + CellValue,
    V: CellValue,
    U: CellValue,
    F: Fn(&K, &V) -> U + Send + Sync + 'static,
{
    fn build_into(
        self,
        cx: &mut crate::map_query::compiler::CompileContext,
        sink: crate::map_query::BoxedMapDiffSink<K, U>,
    ) -> Vec<SubscriptionGuard> {
        install_map_values_runtime(cx, self.source, self.f, sink)
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

impl<S, K, V, U, F> MapValuesPlan<S, K, V, U, F>
where
    S: MapQuery<Key = K, Value = V>,
    K: Hash + Eq + CellValue,
    V: CellValue,
    U: CellValue,
    F: Fn(&K, &V) -> U + Send + Sync + 'static,
{
    /// Fuse another exactly-one projection into this kernel.
    pub fn map_values<W, G>(
        self,
        g: G,
    ) -> MapValuesPlan<S, K, V, W, impl Fn(&K, &V) -> W + Send + Sync + 'static>
    where
        W: CellValue,
        G: Fn(&K, &U) -> W + Send + Sync + 'static,
    {
        let f = self.f;
        MapValuesPlan {
            source: self.source,
            f: move |key, value| {
                let value = f(key, value);
                g(key, &value)
            },
            _types: PhantomData,
        }
    }

    /// Fuse a filtering projection into this kernel.
    pub fn filter_map_values<W, G>(
        self,
        g: G,
    ) -> FilterMapValuesPlan<S, K, V, W, impl Fn(&K, &V) -> Option<W> + Send + Sync + 'static>
    where
        W: CellValue,
        G: Fn(&K, &U) -> Option<W> + Send + Sync + 'static,
    {
        let f = self.f;
        FilterMapValuesPlan {
            source: self.source,
            f: move |key, value| {
                let value = f(key, value);
                g(key, &value)
            },
            _types: PhantomData,
        }
    }

    /// Fuse a value predicate after this projection.
    pub fn select<G>(
        self,
        predicate: G,
    ) -> FilterMapValuesPlan<S, K, V, U, impl Fn(&K, &V) -> Option<U> + Send + Sync + 'static>
    where
        G: Fn(&U) -> bool + Send + Sync + 'static,
    {
        let f = self.f;
        FilterMapValuesPlan {
            source: self.source,
            f: move |key, value| {
                let value = f(key, value);
                predicate(&value).then_some(value)
            },
            _types: PhantomData,
        }
    }

    /// Fuse a key-aware predicate after this projection.
    pub fn select_by<G>(
        self,
        predicate: G,
    ) -> FilterMapValuesPlan<S, K, V, U, impl Fn(&K, &V) -> Option<U> + Send + Sync + 'static>
    where
        G: Fn(&K, &U) -> bool + Send + Sync + 'static,
    {
        let f = self.f;
        FilterMapValuesPlan {
            source: self.source,
            f: move |key, value| {
                let value = f(key, value);
                predicate(key, &value).then_some(value)
            },
            _types: PhantomData,
        }
    }
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
    pub(crate) source: S,
    pub(crate) f: F,
    pub(crate) _types: PhantomData<fn() -> (K, V, U)>,
}

impl<S, K, V, U, F> FilterMapValuesPlan<S, K, V, U, F>
where
    S: MapQuery<Key = K, Value = V>,
    K: Hash + Eq + CellValue,
    V: CellValue,
    U: CellValue,
    F: Fn(&K, &V) -> Option<U> + Send + Sync + 'static,
{
    /// Fuse an exactly-one projection after this optional kernel.
    pub fn map_values<W, G>(
        self,
        g: G,
    ) -> FilterMapValuesPlan<S, K, V, W, impl Fn(&K, &V) -> Option<W> + Send + Sync + 'static>
    where
        W: CellValue,
        G: Fn(&K, &U) -> W + Send + Sync + 'static,
    {
        let f = self.f;
        FilterMapValuesPlan {
            source: self.source,
            f: move |key, value| f(key, value).map(|value| g(key, &value)),
            _types: PhantomData,
        }
    }

    /// Fuse another filtering projection into this optional kernel.
    pub fn filter_map_values<W, G>(
        self,
        g: G,
    ) -> FilterMapValuesPlan<S, K, V, W, impl Fn(&K, &V) -> Option<W> + Send + Sync + 'static>
    where
        W: CellValue,
        G: Fn(&K, &U) -> Option<W> + Send + Sync + 'static,
    {
        let f = self.f;
        FilterMapValuesPlan {
            source: self.source,
            f: move |key, value| f(key, value).and_then(|value| g(key, &value)),
            _types: PhantomData,
        }
    }

    /// Fuse a value predicate into this optional kernel.
    pub fn select<G>(
        self,
        predicate: G,
    ) -> FilterMapValuesPlan<S, K, V, U, impl Fn(&K, &V) -> Option<U> + Send + Sync + 'static>
    where
        G: Fn(&U) -> bool + Send + Sync + 'static,
    {
        let f = self.f;
        FilterMapValuesPlan {
            source: self.source,
            f: move |key, value| f(key, value).filter(|value| predicate(value)),
            _types: PhantomData,
        }
    }

    /// Fuse a key-aware predicate into this optional kernel.
    pub fn select_by<G>(
        self,
        predicate: G,
    ) -> FilterMapValuesPlan<S, K, V, U, impl Fn(&K, &V) -> Option<U> + Send + Sync + 'static>
    where
        G: Fn(&K, &U) -> bool + Send + Sync + 'static,
    {
        let f = self.f;
        FilterMapValuesPlan {
            source: self.source,
            f: move |key, value| f(key, value).filter(|value| predicate(key, value)),
            _types: PhantomData,
        }
    }
}

impl<S, K, V, U, F> BuildQueryRuntime<K, U> for FilterMapValuesPlan<S, K, V, U, F>
where
    S: MapQuery<Key = K, Value = V>,
    K: Hash + Eq + CellValue,
    V: CellValue,
    U: CellValue,
    F: Fn(&K, &V) -> Option<U> + Send + Sync + 'static,
{
    fn build_into(
        self,
        cx: &mut crate::map_query::compiler::CompileContext,
        sink: crate::map_query::BoxedMapDiffSink<K, U>,
    ) -> Vec<SubscriptionGuard> {
        install_filter_map_values_runtime(cx, self.source, self.f, sink)
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
///
/// Projection closures must be deterministic, externally side-effect-free,
/// and nonblocking. They may be invoked repeatedly or concurrently; invocation
/// count, order, and thread are not API guarantees.
pub trait MapValuesExt<K, V>: MapQuery<Key = K, Value = V>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    /// Map every value while retaining its input key.
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

    /// Map or omit a value while retaining its input key.
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
