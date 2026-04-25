//! Project-many plan node implementing [`MapQuery`].
//!
//! `project_many` builds an uncompiled plan node that composes with other
//! [`MapQuery`] operators. Call [`MapQuery::materialize`] to compile a plan
//! into a subscribable [`CellMap`](crate::CellMap).

use std::{hash::Hash, marker::PhantomData, sync::Arc};

use crate::{
    map_query::{MapDiffSink, MapQuery, MapQueryInstall},
    subscription::SubscriptionGuard,
    traits::{
        CellValue,
        collections::internal::map_runtime::install_map_runtime_via_query,
    },
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
    S: MapQuery<SK, SV>,
    SK: Hash + Eq + CellValue,
    SV: CellValue,
    OK: Hash + Eq + CellValue,
    OV: CellValue,
    F: Fn(&SK, &SV) -> Vec<(OK, OV)> + Send + Sync + 'static,
{
    pub(crate) source: S,
    pub(crate) f: Arc<F>,
    #[allow(clippy::type_complexity)]
    pub(crate) _types: PhantomData<fn() -> (SK, SV, OK, OV)>,
}

impl<S, SK, SV, OK, OV, F> MapQueryInstall<OK, OV> for ProjectManyPlan<S, SK, SV, OK, OV, F>
where
    S: MapQuery<SK, SV>,
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
impl<S, SK, SV, OK, OV, F> MapQuery<OK, OV> for ProjectManyPlan<S, SK, SV, OK, OV, F>
where
    S: MapQuery<SK, SV>,
    SK: Hash + Eq + CellValue,
    SV: CellValue,
    OK: Hash + Eq + CellValue,
    OV: CellValue,
    F: Fn(&SK, &SV) -> Vec<(OK, OV)> + Send + Sync + 'static,
{
}

/// Project-many operator returning a [`MapQuery`] plan node.
///
/// `project_many` consumes `self` and returns an uncompiled plan node; call
/// [`MapQuery::materialize`] on the result to obtain a subscribable
/// [`CellMap`](crate::CellMap).
pub trait ProjectManyExt<K, V>: MapQuery<K, V>
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
    fn project_many<K2, V2, F>(self, f: F) -> ProjectManyPlan<Self, K, V, K2, V2, F>
    where
        K2: Hash + Eq + CellValue,
        V2: CellValue,
        F: Fn(&K, &V) -> Vec<(K2, V2)> + Send + Sync + 'static,
    {
        ProjectManyPlan {
            source: self,
            f: Arc::new(f),
            _types: PhantomData,
        }
    }
}

impl<K, V, M> ProjectManyExt<K, V> for M
where
    K: Hash + Eq + CellValue,
    V: CellValue,
    M: MapQuery<K, V>,
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
}
