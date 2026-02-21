use std::hash::Hash;

use super::{ProjectMapExt, internal::map_values_cell::map_values_cell};
use crate::{
    cell::CellImmutable,
    cell_map::CellMap,
    traits::{CellValue, Gettable, Watchable},
};

pub trait ProjectCellExt<K, V>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    /// Reactive project variant: each row maps to a watchable optional output row.
    ///
    /// `mapper(&source_key, &source_value)` returns a watchable `Option<(output_key, output_value)>`.
    /// `None` means the row is excluded.
    #[track_caller]
    fn project_cell<K2, V2, W, F>(&self, mapper: F) -> CellMap<K2, V2, CellImmutable>
    where
        K2: Hash + Eq + CellValue,
        V2: CellValue,
        W: Watchable<Option<(K2, V2)>> + Gettable<Option<(K2, V2)>> + Clone + Send + Sync + 'static,
        F: Fn(&K, &V) -> W + Send + Sync + 'static;
}

impl<K, V, M> ProjectCellExt<K, V> for CellMap<K, V, M>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    fn project_cell<K2, V2, W, F>(&self, mapper: F) -> CellMap<K2, V2, CellImmutable>
    where
        K2: Hash + Eq + CellValue,
        V2: CellValue,
        W: Watchable<Option<(K2, V2)>> + Gettable<Option<(K2, V2)>> + Clone + Send + Sync + 'static,
        F: Fn(&K, &V) -> W + Send + Sync + 'static,
    {
        map_values_cell(self, mapper)
            .project(|_, row| row.as_ref().map(|(k, v)| (k.clone(), v.clone())))
    }
}
