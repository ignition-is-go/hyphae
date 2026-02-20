use std::hash::Hash;

use crate::{
    cell::CellImmutable,
    cell_map::CellMap,
    traits::{CellValue, collections::internal::join_runtime::run_join_runtime},
};

pub trait LeftJoinOnByExt<K, V>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    /// Left-joins this map against `lookup` and keeps left rows where `predicate` is true.
    ///
    /// `lookup_key` picks the lookup row; `predicate` receives `Some(&right)` when present,
    /// otherwise `None`.
    fn left_join_on_by<LK, R, LM, FK, FP>(
        &self,
        lookup: &CellMap<LK, R, LM>,
        lookup_key: FK,
        predicate: FP,
    ) -> CellMap<K, V, CellImmutable>
    where
        LK: Hash + Eq + CellValue,
        R: CellValue,
        LM: Clone + Send + Sync + 'static,
        FK: Fn(&K, &V) -> LK + Send + Sync + 'static,
        FP: Fn(&K, &V, Option<&R>) -> bool + Send + Sync + 'static;
}

impl<K, V, M> LeftJoinOnByExt<K, V> for CellMap<K, V, M>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    fn left_join_on_by<LK, R, LM, FK, FP>(
        &self,
        lookup: &CellMap<LK, R, LM>,
        lookup_key: FK,
        predicate: FP,
    ) -> CellMap<K, V, CellImmutable>
    where
        LK: Hash + Eq + CellValue,
        R: CellValue,
        LM: Clone + Send + Sync + 'static,
        FK: Fn(&K, &V) -> LK + Send + Sync + 'static,
        FP: Fn(&K, &V, Option<&R>) -> bool + Send + Sync + 'static,
    {
        run_join_runtime(
            self,
            lookup,
            "left_join_on_by",
            lookup_key,
            |lookup_k, _| lookup_k.clone(),
            move |left_k, left_v, rights| {
                let right = rights.first().map(|(_, right_value)| right_value);
                if predicate(left_k, left_v, right) {
                    vec![(left_k.clone(), left_v.clone())]
                } else {
                    Vec::new()
                }
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
    fn left_join_on_by_preserves_lookup_batch_without_extra_emissions() {
        let left = CellMap::<String, i32>::new();
        left.insert("a".to_string(), 1);
        left.insert("b".to_string(), 2);

        let lookup = CellMap::<String, bool>::new();
        let out = left.left_join_on_by(
            &lookup,
            |_, v| v.to_string(),
            |_, _, present| present.copied().unwrap_or(false),
        );

        let (tx, rx) = mpsc::channel::<MapDiff<String, i32>>();
        let _guard = out.subscribe_diffs(move |diff| {
            let _ = tx.send(diff.clone());
        });

        lookup.insert_many(vec![("1".to_string(), true), ("2".to_string(), true)]);
        let seen: Vec<_> = rx.try_iter().collect();
        assert_eq!(seen.len(), 2);
        match seen.last().unwrap() {
            MapDiff::Batch { changes } => assert_eq!(changes.len(), 2),
            _ => panic!("expected batch diff from left_join_on_by"),
        }
    }
}
