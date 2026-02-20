use std::hash::Hash;

use crate::{
    cell::CellImmutable,
    cell_map::CellMap,
    traits::{CellValue, collections::internal::map_runtime::run_map_runtime},
};

pub trait FlatMapMapExt<K, V>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    /// Expands each source row into zero or more output rows.
    ///
    /// `f(&source_key, &source_value)` returns the full set of `(output_key, output_value)` pairs
    /// that should exist for that source row.
    #[track_caller]
    fn flat_map<K2, V2, F>(&self, f: F) -> CellMap<K2, V2, CellImmutable>
    where
        K2: Hash + Eq + CellValue,
        V2: CellValue,
        F: Fn(&K, &V) -> Vec<(K2, V2)> + Send + Sync + 'static;
}

impl<K, V, M> FlatMapMapExt<K, V> for CellMap<K, V, M>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    fn flat_map<K2, V2, F>(&self, f: F) -> CellMap<K2, V2, CellImmutable>
    where
        K2: Hash + Eq + CellValue,
        V2: CellValue,
        F: Fn(&K, &V) -> Vec<(K2, V2)> + Send + Sync + 'static,
    {
        run_map_runtime(self, "flat_map", move |k, v| f(k, v))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flat_map_update_removes_old_then_inserts_new() {
        let source = CellMap::<String, i32>::new();
        let out = source.flat_map(|key, value| {
            vec![
                (format!("{key}:x"), value * 10),
                (format!("{key}:y"), value * 100),
            ]
        });

        source.insert("a".to_string(), 1);
        assert_eq!(out.get_value(&"a:x".to_string()), Some(10));
        assert_eq!(out.get_value(&"a:y".to_string()), Some(100));
        source.insert("a".to_string(), 2);
        assert_eq!(out.get_value(&"a:x".to_string()), Some(20));
        assert_eq!(out.get_value(&"a:y".to_string()), Some(200));
    }
}
