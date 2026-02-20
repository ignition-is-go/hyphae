use std::hash::Hash;

use crate::{
    cell::CellImmutable,
    cell_map::CellMap,
    traits::{CellValue, collections::internal::map_runtime::run_map_runtime},
};

pub trait ProjectMapExt<K, V>
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
    fn project<K2, V2, F>(&self, f: F) -> CellMap<K2, V2, CellImmutable>
    where
        K2: Hash + Eq + CellValue,
        V2: CellValue,
        F: Fn(&K, &V) -> Option<(K2, V2)> + Send + Sync + 'static;
}

impl<K, V, M> ProjectMapExt<K, V> for CellMap<K, V, M>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    fn project<K2, V2, F>(&self, f: F) -> CellMap<K2, V2, CellImmutable>
    where
        K2: Hash + Eq + CellValue,
        V2: CellValue,
        F: Fn(&K, &V) -> Option<(K2, V2)> + Send + Sync + 'static,
    {
        run_map_runtime(self, "project", move |k, v| f(k, v).into_iter().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_handles_projection_disappearance_without_extra_rows() {
        let source = CellMap::<String, i32>::new();
        let projected = source.project(|key, value| {
            if *value > 0 {
                Some((format!("p:{key}"), value * 10))
            } else {
                None
            }
        });

        source.insert("a".to_string(), 1);
        assert_eq!(projected.get_value(&"p:a".to_string()), Some(10));
        source.insert("a".to_string(), -1);
        assert_eq!(projected.get_value(&"p:a".to_string()), None);
    }
}
