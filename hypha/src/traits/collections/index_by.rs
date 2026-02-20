use std::{collections::HashSet, hash::Hash, sync::Arc};

use crate::{
    cell::CellImmutable,
    cell_map::CellMap,
    traits::{
        CellValue,
        collections::internal::diff_runtime::{GroupedOps, run_grouped_runtime},
    },
};

pub trait IndexByExt<K, V>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    /// Builds an index from a derived join key to the set of source keys in that bucket.
    ///
    /// `f(&source_key, &source_value)` chooses the bucket key.
    #[track_caller]
    fn index_by<JK, F>(&self, f: F) -> CellMap<JK, HashSet<K>, CellImmutable>
    where
        JK: Hash + Eq + CellValue,
        F: Fn(&K, &V) -> JK + Send + Sync + 'static;
}

impl<K, V, M> IndexByExt<K, V> for CellMap<K, V, M>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    fn index_by<JK, F>(&self, f: F) -> CellMap<JK, HashSet<K>, CellImmutable>
    where
        JK: Hash + Eq + CellValue,
        F: Fn(&K, &V) -> JK + Send + Sync + 'static,
    {
        let f = Arc::new(f);
        run_grouped_runtime(
            self,
            "index_by",
            GroupedOps {
                make_group_key: move |k, v| f(k, v),
                on_insert: |bucket: &mut HashSet<K>, key: &K, _: &V| {
                    bucket.insert(key.clone());
                },
                on_update: |bucket: &mut HashSet<K>, key: &K, _: &V, _: &V| {
                    bucket.insert(key.clone());
                },
                on_remove: |bucket: &mut HashSet<K>, key: &K, _: &V| {
                    bucket.remove(key);
                },
                materialize: |bucket: &HashSet<K>| bucket.clone(),
                is_empty: |bucket: &HashSet<K>| bucket.is_empty(),
                new_group_state: HashSet::new,
                _marker: std::marker::PhantomData,
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn index_by_moves_buckets_on_join_key_change_without_extra_rows() {
        let source = CellMap::<String, i32>::new();
        let index = source.index_by(|_, value| {
            if value % 2 == 0 {
                "even".to_string()
            } else {
                "odd".to_string()
            }
        });

        source.insert("a".to_string(), 1);
        assert_eq!(
            index.get_value(&"odd".to_string()),
            Some(HashSet::from(["a".to_string()]))
        );
        assert_eq!(index.get_value(&"even".to_string()), None);

        source.insert("a".to_string(), 2);
        assert_eq!(index.get_value(&"odd".to_string()), None);
        assert_eq!(
            index.get_value(&"even".to_string()),
            Some(HashSet::from(["a".to_string()]))
        );
    }
}
