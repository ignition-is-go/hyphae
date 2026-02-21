use std::hash::Hash;

use crate::{
    cell::CellImmutable,
    cell_map::CellMap,
    traits::{CellValue, collections::internal::join_runtime::run_join_runtime},
};

pub trait LeftSemiJoinMapOnByExt<K, V>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    /// Keeps left rows with at least one matching right row by join key, then maps those rows.
    ///
    /// `left_key` and `right_key` extract the join key for each side.
    fn left_semi_join_map_on_by<RK, RV, RM, JK, FL, FR, U, FM>(
        &self,
        right: &CellMap<RK, RV, RM>,
        left_key: FL,
        right_key: FR,
        mapper: FM,
    ) -> CellMap<K, U, CellImmutable>
    where
        RK: Hash + Eq + CellValue,
        RV: CellValue,
        JK: Hash + Eq + CellValue,
        U: CellValue,
        FL: Fn(&K, &V) -> JK + Send + Sync + 'static,
        FR: Fn(&RK, &RV) -> JK + Send + Sync + 'static,
        FM: Fn(&K, &V, &RV) -> U + Send + Sync + 'static;
}

impl<K, V, M> LeftSemiJoinMapOnByExt<K, V> for CellMap<K, V, M>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    fn left_semi_join_map_on_by<RK, RV, RM, JK, FL, FR, U, FM>(
        &self,
        right: &CellMap<RK, RV, RM>,
        left_key: FL,
        right_key: FR,
        mapper: FM,
    ) -> CellMap<K, U, CellImmutable>
    where
        RK: Hash + Eq + CellValue,
        RV: CellValue,
        JK: Hash + Eq + CellValue,
        U: CellValue,
        FL: Fn(&K, &V) -> JK + Send + Sync + 'static,
        FR: Fn(&RK, &RV) -> JK + Send + Sync + 'static,
        FM: Fn(&K, &V, &RV) -> U + Send + Sync + 'static,
    {
        run_join_runtime(
            self,
            right,
            "left_semi_join_map_on_by",
            left_key,
            right_key,
            move |left_k, left_v, rights| {
                let Some((_, right_v)) = rights.first() else {
                    return Vec::new();
                };
                vec![(left_k.clone(), mapper(left_k, left_v, right_v))]
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
    fn left_semi_join_map_on_by_filters_and_maps() {
        let left = CellMap::<String, i32>::new();
        let right = CellMap::<String, i32>::new();
        let out = left.left_semi_join_map_on_by(
            &right,
            |_, lv| lv.to_string(),
            |rk, _| rk.clone(),
            |_, lv, rv| lv + rv,
        );

        left.insert("a".to_string(), 10);
        assert_eq!(out.get_value(&"a".to_string()), None);
        right.insert("10".to_string(), 7);
        assert_eq!(out.get_value(&"a".to_string()), Some(17));
    }

    #[test]
    fn left_semi_join_map_on_by_right_batch_emits_single_batch() {
        let left = CellMap::<String, i32>::new();
        let right = CellMap::<String, i32>::new();
        left.insert("a".to_string(), 1);
        left.insert("b".to_string(), 2);
        let out = left.left_semi_join_map_on_by(
            &right,
            |_, lv| lv.to_string(),
            |rk, _| rk.clone(),
            |_, lv, rv| lv + rv,
        );

        let (tx, rx) = mpsc::channel::<MapDiff<String, i32>>();
        let _guard = out.subscribe_diffs(move |diff| {
            let _ = tx.send(diff.clone());
        });

        right.insert_many(vec![("1".to_string(), 10), ("2".to_string(), 20)]);

        let seen: Vec<_> = rx.try_iter().collect();
        assert_eq!(seen.len(), 2);
        match seen.last().expect("last diff") {
            MapDiff::Batch { changes } => assert_eq!(changes.len(), 2),
            _ => panic!("expected batch diff from left_semi_join_map_on_by"),
        }
    }
}
