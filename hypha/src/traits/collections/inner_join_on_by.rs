use std::hash::Hash;

use crate::{
    cell::CellImmutable,
    cell_map::CellMap,
    traits::{CellValue, collections::internal::join_runtime::run_join_runtime},
};

pub trait InnerJoinOnByExt<K, V>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    /// Performs an inner join using explicit key extractors.
    ///
    /// `left_key` and `right_key` extract the join key from each side.
    /// For every matching pair, `compute` returns one `(output_key, output_value)` row.
    fn inner_join_on_by<RK, RV, RM, JK, OK, OV, FL, FR, FC>(
        &self,
        right: &CellMap<RK, RV, RM>,
        left_key: FL,
        right_key: FR,
        compute: FC,
    ) -> CellMap<OK, OV, CellImmutable>
    where
        RK: Hash + Eq + CellValue,
        RV: CellValue,
        JK: Hash + Eq + CellValue,
        OK: Hash + Eq + CellValue,
        OV: CellValue,
        FL: Fn(&K, &V) -> JK + Send + Sync + 'static,
        FR: Fn(&RK, &RV) -> JK + Send + Sync + 'static,
        FC: Fn(&K, &V, &RK, &RV) -> (OK, OV) + Send + Sync + 'static;
}

impl<K, V, M> InnerJoinOnByExt<K, V> for CellMap<K, V, M>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    fn inner_join_on_by<RK, RV, RM, JK, OK, OV, FL, FR, FC>(
        &self,
        right: &CellMap<RK, RV, RM>,
        left_key: FL,
        right_key: FR,
        compute: FC,
    ) -> CellMap<OK, OV, CellImmutable>
    where
        RK: Hash + Eq + CellValue,
        RV: CellValue,
        JK: Hash + Eq + CellValue,
        OK: Hash + Eq + CellValue,
        OV: CellValue,
        FL: Fn(&K, &V) -> JK + Send + Sync + 'static,
        FR: Fn(&RK, &RV) -> JK + Send + Sync + 'static,
        FC: Fn(&K, &V, &RK, &RV) -> (OK, OV) + Send + Sync + 'static,
    {
        run_join_runtime(
            self,
            right,
            "inner_join_on_by",
            left_key,
            right_key,
            move |left_k, left_v, rights| {
                rights
                    .iter()
                    .map(|(right_k, right_v)| compute(left_k, left_v, right_k, right_v))
                    .collect()
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
    fn inner_join_on_by_preserves_right_batch_without_extra_emissions() {
        let left = CellMap::<String, (String, i32)>::new();
        left.insert("l1".to_string(), ("g1".to_string(), 10));

        let right = CellMap::<String, (String, i32)>::new();
        let joined = left.inner_join_on_by(
            &right,
            |_, left_v| left_v.0.clone(),
            |_, right_v| right_v.0.clone(),
            |left_k, left_v, right_k, right_v| {
                (format!("{left_k}:{right_k}"), left_v.1 + right_v.1)
            },
        );

        let (tx, rx) = mpsc::channel::<MapDiff<String, i32>>();
        let _guard = joined.subscribe_diffs(move |diff| {
            let _ = tx.send(diff.clone());
        });

        right.insert_many(vec![
            ("r1".to_string(), ("g1".to_string(), 5)),
            ("r2".to_string(), ("g1".to_string(), 7)),
        ]);

        let seen: Vec<_> = rx.try_iter().collect();
        assert_eq!(seen.len(), 2);
        match seen.last().expect("last diff") {
            MapDiff::Batch { changes } => assert_eq!(changes.len(), 2),
            _ => panic!("expected batch diff from inner_join_on_by"),
        }
    }
}
