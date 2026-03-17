use std::{hash::Hash, sync::Arc};

use crate::{
    cell::CellImmutable,
    cell_map::CellMap,
    traits::{
        CellValue,
        collections::internal::diff_runtime::{GroupedOps, run_grouped_runtime},
    },
};

pub trait CountByExt<K, V>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    /// Groups rows by `group_key(&source_key, &source_value)` and stores how many rows are in each group.
    ///
    /// `group_key` is called for every source row and its return value becomes the output map key.
    #[track_caller]
    fn count_by<GK, F>(&self, group_key: F) -> CellMap<GK, usize, CellImmutable>
    where
        GK: Hash + Eq + CellValue,
        F: Fn(&K, &V) -> GK + Send + Sync + 'static;
}

impl<K, V, M> CountByExt<K, V> for CellMap<K, V, M>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    fn count_by<GK, F>(&self, group_key: F) -> CellMap<GK, usize, CellImmutable>
    where
        GK: Hash + Eq + CellValue,
        F: Fn(&K, &V) -> GK + Send + Sync + 'static,
    {
        let group_key = Arc::new(group_key);
        run_grouped_runtime(
            self,
            "count_by",
            GroupedOps {
                make_group_key: move |k, v| group_key(k, v),
                on_insert: |group_count: &mut usize, _: &K, _: &V| {
                    *group_count += 1;
                },
                on_update: |_: &mut usize, _: &K, _: &V, _: &V| {},
                on_remove: |group_count: &mut usize, _: &K, _: &V| {
                    *group_count = group_count.saturating_sub(1);
                },
                materialize: |group_count: &usize| *group_count,
                is_empty: |group_count: &usize| *group_count == 0,
                new_group_state: || 0usize,
                _marker: std::marker::PhantomData,
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;
    use crate::MapDiff;

    #[test]
    fn count_by_preserves_upstream_batch_without_extra_emissions() {
        let source = CellMap::<String, i32>::new();
        let counts = source.count_by(|_, v| v.to_string());

        let (tx, rx) = mpsc::channel::<MapDiff<String, usize>>();
        let _guard = counts.subscribe_diffs(move |diff| {
            let _ = tx.send(diff.clone());
        });

        source.insert_many(vec![("a".to_string(), 1), ("b".to_string(), 2)]);
        let seen: Vec<_> = rx.try_iter().collect();
        assert_eq!(seen.len(), 2);
        match seen.last().unwrap() {
            MapDiff::Batch { changes } => assert_eq!(changes.len(), 2),
            _ => panic!("expected batch diff from count_by"),
        }
    }
}
