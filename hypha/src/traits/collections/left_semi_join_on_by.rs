use std::hash::Hash;

use crate::{
    cell::CellImmutable,
    cell_map::CellMap,
    traits::{CellValue, collections::internal::join_runtime::run_join_runtime},
};

pub trait LeftSemiJoinOnByExt<K, V>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    /// Keeps left rows that have at least one right row with the same join key.
    ///
    /// `left_key` and `right_key` extract join keys from each side.
    fn left_semi_join_on_by<RK, RV, RM, JK, FL, FR>(
        &self,
        right: &CellMap<RK, RV, RM>,
        left_key: FL,
        right_key: FR,
    ) -> CellMap<K, V, CellImmutable>
    where
        RK: Hash + Eq + CellValue,
        RV: CellValue,
        JK: Hash + Eq + CellValue,
        FL: Fn(&K, &V) -> JK + Send + Sync + 'static,
        FR: Fn(&RK, &RV) -> JK + Send + Sync + 'static;
}

impl<K, V, M> LeftSemiJoinOnByExt<K, V> for CellMap<K, V, M>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    fn left_semi_join_on_by<RK, RV, RM, JK, FL, FR>(
        &self,
        right: &CellMap<RK, RV, RM>,
        left_key: FL,
        right_key: FR,
    ) -> CellMap<K, V, CellImmutable>
    where
        RK: Hash + Eq + CellValue,
        RV: CellValue,
        JK: Hash + Eq + CellValue,
        FL: Fn(&K, &V) -> JK + Send + Sync + 'static,
        FR: Fn(&RK, &RV) -> JK + Send + Sync + 'static,
    {
        run_join_runtime(
            self,
            right,
            "left_semi_join_on_by",
            left_key,
            right_key,
            move |left_k, left_v, rights| {
                if rights.is_empty() {
                    Vec::new()
                } else {
                    vec![(left_k.clone(), left_v.clone())]
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
    fn left_semi_join_on_by_preserves_right_batch_without_extra_emissions() {
        let left = CellMap::<String, i32>::new();
        left.insert("a".to_string(), 1);
        left.insert("b".to_string(), 2);

        let right = CellMap::<String, bool>::new();
        let out =
            left.left_semi_join_on_by(&right, |_, value| value.to_string(), |key, _| key.clone());

        let (tx, rx) = mpsc::channel::<MapDiff<String, i32>>();
        let _guard = out.subscribe_diffs(move |diff| {
            let _ = tx.send(diff.clone());
        });

        right.insert_many(vec![("1".to_string(), true), ("2".to_string(), true)]);

        let seen: Vec<_> = rx.try_iter().collect();
        assert_eq!(seen.len(), 2);
        match seen.last().expect("last diff") {
            MapDiff::Batch { changes } => assert_eq!(changes.len(), 2),
            _ => panic!("expected batch diff from left_semi_join_on_by"),
        }
    }
}
