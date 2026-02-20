use std::{collections::BTreeMap, hash::Hash, sync::Arc};

use crate::{
    cell::CellImmutable,
    cell_map::CellMap,
    traits::{
        CellValue,
        collections::internal::diff_runtime::{GroupedOps, run_grouped_runtime},
    },
};

pub trait GroupByExt<K, V>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    /// Groups source rows by `group_key(&source_key, &source_value)`.
    ///
    /// Each output key is a group id and each output value is the group's rows as `Vec<V>`.
    #[track_caller]
    fn group_by<GK, F>(&self, group_key: F) -> CellMap<GK, Vec<V>, CellImmutable>
    where
        K: Ord,
        GK: Hash + Eq + CellValue,
        F: Fn(&K, &V) -> GK + Send + Sync + 'static;
}

impl<K, V, M> GroupByExt<K, V> for CellMap<K, V, M>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    fn group_by<GK, F>(&self, group_key: F) -> CellMap<GK, Vec<V>, CellImmutable>
    where
        K: Ord,
        GK: Hash + Eq + CellValue,
        F: Fn(&K, &V) -> GK + Send + Sync + 'static,
    {
        let group_key = Arc::new(group_key);
        run_grouped_runtime(
            self,
            "group_by",
            GroupedOps {
                make_group_key: move |k, v| group_key(k, v),
                on_insert: |rows: &mut BTreeMap<K, V>, key: &K, value: &V| {
                    rows.insert(key.clone(), value.clone());
                },
                on_update: |rows: &mut BTreeMap<K, V>, key: &K, _: &V, new_value: &V| {
                    rows.insert(key.clone(), new_value.clone());
                },
                on_remove: |rows: &mut BTreeMap<K, V>, key: &K, _: &V| {
                    rows.remove(key);
                },
                materialize: |rows: &BTreeMap<K, V>| rows.values().cloned().collect::<Vec<_>>(),
                is_empty: |rows: &BTreeMap<K, V>| rows.is_empty(),
                new_group_state: BTreeMap::new,
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
    fn group_by_preserves_upstream_batch_without_extra_emissions() {
        let source = CellMap::<String, i32>::new();
        let grouped = source.group_by(|_, v| v.to_string());

        let (tx, rx) = mpsc::channel::<MapDiff<String, Vec<i32>>>();
        let _guard = grouped.subscribe_diffs(move |diff| {
            let _ = tx.send(diff.clone());
        });

        source.insert_many(vec![("a".to_string(), 1), ("b".to_string(), 2)]);
        let seen: Vec<_> = rx.try_iter().collect();
        assert_eq!(seen.len(), 2);
        match seen.last().unwrap() {
            MapDiff::Batch { changes } => assert_eq!(changes.len(), 2),
            _ => panic!("expected batch diff from group_by"),
        }
    }
}
