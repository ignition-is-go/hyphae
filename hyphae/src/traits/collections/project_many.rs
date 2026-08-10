//! Project-many plan node implementing [`MapQuery`].
//!
//! `project_many` builds an uncompiled plan node that composes with other
//! [`MapQuery`] operators. Call [`MapQuery::materialize`] to compile a plan
//! into a subscribable [`CellMap`](crate::CellMap).

use std::{hash::Hash, marker::PhantomData};

use crate::{
    map_query::{MapDiffSink, MapQuery, MapQueryInstall},
    subscription::SubscriptionGuard,
    traits::{CellValue, collections::internal::map_runtime::install_map_runtime_via_query},
};

/// Plan node for [`ProjectManyExt::project_many`].
///
/// Each source row maps to zero, one, or many output rows. The closure
/// returns all output rows currently produced by that source row; changes
/// are diffed automatically against the previous output for the same source
/// row.
///
/// Not [`Clone`]: cloning a plan would silently duplicate projection work;
/// share by materializing once.
pub struct ProjectManyPlan<S, SK, SV, OK, OV, F>
where
    S: MapQuery<Key = SK, Value = SV>,
    SK: Hash + Eq + CellValue,
    SV: CellValue,
    OK: Hash + Eq + CellValue,
    OV: CellValue,
    F: Fn(&SK, &SV) -> Vec<(OK, OV)> + Send + Sync + 'static,
{
    pub(crate) source: S,
    pub(crate) f: F,
    #[allow(clippy::type_complexity)]
    pub(crate) _types: PhantomData<fn() -> (SK, SV, OK, OV)>,
}

impl<S, SK, SV, OK, OV, F> MapQueryInstall<OK, OV> for ProjectManyPlan<S, SK, SV, OK, OV, F>
where
    S: MapQuery<Key = SK, Value = SV>,
    SK: Hash + Eq + CellValue,
    SV: CellValue,
    OK: Hash + Eq + CellValue,
    OV: CellValue,
    F: Fn(&SK, &SV) -> Vec<(OK, OV)> + Send + Sync + 'static,
{
    fn install(self, sink: MapDiffSink<OK, OV>) -> Vec<SubscriptionGuard> {
        let f = self.f;
        install_map_runtime_via_query::<SK, SV, OK, OV, S, _>(
            self.source,
            move |k, v| f(k, v),
            sink,
        )
    }
}

#[allow(private_bounds)]
impl<S, SK, SV, OK, OV, F> MapQuery for ProjectManyPlan<S, SK, SV, OK, OV, F>
where
    S: MapQuery<Key = SK, Value = SV>,
    SK: Hash + Eq + CellValue,
    SV: CellValue,
    OK: Hash + Eq + CellValue,
    OV: CellValue,
    F: Fn(&SK, &SV) -> Vec<(OK, OV)> + Send + Sync + 'static,
{
    type Key = OK;
    type Value = OV;
}

/// Project-many operator returning a [`MapQuery`] plan node.
///
/// `project_many` consumes `self` and returns an uncompiled plan node; call
/// [`MapQuery::materialize`] on the result to obtain a subscribable
/// [`CellMap`](crate::CellMap).
pub trait ProjectManyExt<K, V>: MapQuery<Key = K, Value = V>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    /// Projects each source row to zero, one, or many output rows.
    ///
    /// `f(&source_key, &source_value)` returns all output rows currently
    /// produced by that source row. Changes are diffed automatically against
    /// previous output for the same source row.
    #[track_caller]
    fn project_many<K2, V2, F>(self, f: F) -> impl MapQuery<Key = K2, Value = V2>
    where
        K2: Hash + Eq + CellValue,
        V2: CellValue,
        F: Fn(&K, &V) -> Vec<(K2, V2)> + Send + Sync + 'static,
    {
        ProjectManyPlan {
            source: self,
            f,
            _types: PhantomData,
        }
    }
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
    fn install(self, sink: MapDiffSink<(SK, LK), OV>) -> Vec<SubscriptionGuard> {
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

impl<K, V, M> ProjectManyExt<K, V> for M
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
    fn project_many_emits_multiple_rows_per_source() {
        let source = CellMap::<String, i32>::new();
        let out = source
            .clone()
            .project_many(|key, value| {
                if *value <= 0 {
                    return Vec::new();
                }
                vec![
                    (format!("a:{key}"), value * 10),
                    (format!("b:{key}"), value * 100),
                ]
            })
            .materialize();

        source.insert("x".to_string(), 2);
        assert_eq!(out.get_value(&"a:x".to_string()), Some(20));
        assert_eq!(out.get_value(&"b:x".to_string()), Some(200));

        source.insert("x".to_string(), 0);
        assert_eq!(out.get_value(&"a:x".to_string()), None);
        assert_eq!(out.get_value(&"b:x".to_string()), None);
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
