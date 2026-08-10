//! Project plan node implementing [`MapQuery`].
//!
//! `project` builds an uncompiled plan node that composes with other
//! [`MapQuery`] operators. Call [`MapQuery::materialize`] to compile a plan
//! into a subscribable [`CellMap`](crate::CellMap).

use std::{hash::Hash, marker::PhantomData};

use crate::{
    map_query::{MapDiffSink, MapQuery, MapQueryInstall},
    subscription::SubscriptionGuard,
    traits::{CellValue, collections::internal::map_runtime::install_map_runtime_via_query},
};

/// Plan node for [`ProjectMapExt::project`].
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

impl<S, SK, SV, OK, OV, F> MapQueryInstall<OK, OV> for ProjectPlan<S, SK, SV, OK, OV, F>
where
    S: MapQuery<Key = SK, Value = SV>,
    SK: Hash + Eq + CellValue,
    SV: CellValue,
    OK: Hash + Eq + CellValue,
    OV: CellValue,
    F: Fn(&SK, &SV) -> Option<(OK, OV)> + Send + Sync + 'static,
{
    fn install<Sink>(self, sink: Sink) -> Vec<SubscriptionGuard>
    where
        Sink: MapDiffSink<OK, OV>,
    {
        let f = self.f;
        install_map_runtime_via_query::<SK, SV, OK, OV, S, _, _>(
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

/// Project operator returning a [`MapQuery`] plan node.
///
/// `project` consumes `self` and returns an uncompiled plan node; call
/// [`MapQuery::materialize`] on the result to obtain a subscribable
/// [`CellMap`](crate::CellMap).
pub trait ProjectMapExt<K, V>: MapQuery<Key = K, Value = V>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    /// Projects each source row to at most one output row.
    ///
    /// `f(&source_key, &source_value)` returns:
    /// - `Some((output_key, output_value))` to include/update a row
    /// - `None` to remove/exclude that source row from output
    #[track_caller]
    fn project<K2, V2, F>(self, f: F) -> impl MapQuery<Key = K2, Value = V2>
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

impl<S, SK, SV, OK, OV, F> MapQueryInstall<OK, OV> for MapEntriesPlan<S, SK, SV, OK, OV, F>
where
    S: MapQuery<Key = SK, Value = SV>,
    SK: Hash + Eq + CellValue,
    SV: CellValue,
    OK: Hash + Eq + CellValue,
    OV: CellValue,
    F: Fn(&SK, &SV) -> (OK, OV) + Send + Sync + 'static,
{
    fn install<Sink>(self, sink: Sink) -> Vec<SubscriptionGuard>
    where
        Sink: MapDiffSink<OK, OV>,
    {
        let f = self.f;
        install_map_runtime_via_query(self.source, move |k, v| vec![f(k, v)], sink)
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

impl<K, V, M> ProjectMapExt<K, V> for M
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
    fn project_handles_projection_disappearance_without_extra_rows() {
        let source = CellMap::<String, i32>::new();
        let projected = source
            .clone()
            .project(|key, value| {
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
}
