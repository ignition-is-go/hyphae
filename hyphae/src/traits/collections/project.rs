//! Project plan node implementing [`MapQuery`].
//!
//! `project` builds an uncompiled plan node that composes with other
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
    S: MapQuery<SK, SV>,
    SK: Hash + Eq + CellValue,
    SV: CellValue,
    OK: Hash + Eq + CellValue,
    OV: CellValue,
    F: Fn(&SK, &SV) -> Option<(OK, OV)> + Send + Sync + 'static,
{
    pub(crate) source: S,
    pub(crate) f: Arc<F>,
    #[allow(clippy::type_complexity)]
    pub(crate) _types: PhantomData<fn() -> (SK, SV, OK, OV)>,
}

impl<S, SK, SV, OK, OV, F> MapQueryInstall<OK, OV> for ProjectPlan<S, SK, SV, OK, OV, F>
where
    S: MapQuery<SK, SV>,
    SK: Hash + Eq + CellValue,
    SV: CellValue,
    OK: Hash + Eq + CellValue,
    OV: CellValue,
    F: Fn(&SK, &SV) -> Option<(OK, OV)> + Send + Sync + 'static,
{
    fn install(self, sink: MapDiffSink<OK, OV>) -> Vec<SubscriptionGuard> {
        let f = self.f;
        install_map_runtime_via_query::<SK, SV, OK, OV, S, _>(
            self.source,
            move |k, v| f(k, v).into_iter().collect(),
            sink,
        )
    }
}

#[allow(private_bounds)]
impl<S, SK, SV, OK, OV, F> MapQuery<OK, OV> for ProjectPlan<S, SK, SV, OK, OV, F>
where
    S: MapQuery<SK, SV>,
    SK: Hash + Eq + CellValue,
    SV: CellValue,
    OK: Hash + Eq + CellValue,
    OV: CellValue,
    F: Fn(&SK, &SV) -> Option<(OK, OV)> + Send + Sync + 'static,
{
}

/// Project operator returning a [`MapQuery`] plan node.
///
/// `project` consumes `self` and returns an uncompiled plan node; call
/// [`MapQuery::materialize`] on the result to obtain a subscribable
/// [`CellMap`](crate::CellMap).
pub trait ProjectMapExt<K, V>: MapQuery<K, V>
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
    fn project<K2, V2, F>(self, f: F) -> ProjectPlan<Self, K, V, K2, V2, F>
    where
        K2: Hash + Eq + CellValue,
        V2: CellValue,
        F: Fn(&K, &V) -> Option<(K2, V2)> + Send + Sync + 'static,
    {
        ProjectPlan {
            source: self,
            f: Arc::new(f),
            _types: PhantomData,
        }
    }
}

impl<K, V, M> ProjectMapExt<K, V> for M
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
}
