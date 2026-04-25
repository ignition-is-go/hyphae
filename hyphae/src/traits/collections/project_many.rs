use std::hash::Hash;

use crate::{
    cell::CellImmutable,
    cell_map::CellMap,
    traits::{CellValue, collections::internal::map_runtime::run_map_runtime},
};

pub trait ProjectManyExt<K, V>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    /// Projects each source row to zero, one, or many output rows.
    ///
    /// `f(&source_key, &source_value)` returns all output rows currently produced by that source row.
    /// Changes are diffed automatically against previous output for the same source row.
    #[track_caller]
    fn project_many<K2, V2, F>(&self, f: F) -> CellMap<K2, V2, CellImmutable>
    where
        K2: Hash + Eq + CellValue,
        V2: CellValue,
        F: Fn(&K, &V) -> Vec<(K2, V2)> + Send + Sync + 'static;
}

impl<K, V, M> ProjectManyExt<K, V> for CellMap<K, V, M>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
    M: Send + Sync + 'static,
{
    fn project_many<K2, V2, F>(&self, f: F) -> CellMap<K2, V2, CellImmutable>
    where
        K2: Hash + Eq + CellValue,
        V2: CellValue,
        F: Fn(&K, &V) -> Vec<(K2, V2)> + Send + Sync + 'static,
    {
        run_map_runtime(self, "project_many", f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_many_emits_multiple_rows_per_source() {
        let source = CellMap::<String, i32>::new();
        let out = source.project_many(|key, value| {
            if *value <= 0 {
                return Vec::new();
            }
            vec![
                (format!("a:{key}"), value * 10),
                (format!("b:{key}"), value * 100),
            ]
        });

        source.insert("x".to_string(), 2);
        assert_eq!(out.get_value(&"a:x".to_string()), Some(20));
        assert_eq!(out.get_value(&"b:x".to_string()), Some(200));

        source.insert("x".to_string(), 0);
        assert_eq!(out.get_value(&"a:x".to_string()), None);
        assert_eq!(out.get_value(&"b:x".to_string()), None);
    }
}
