use std::hash::Hash;

use crate::{
    cell::CellImmutable,
    cell_map::CellMap,
    traits::{CellValue, collections::internal::join_runtime::run_join_runtime},
};

pub trait LeftSemiJoinExt<K, V>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    /// Left semi join on equal map keys.
    ///
    /// Keeps left rows where a right row with the same key exists.
    /// Unmatched left rows are excluded. Output contains only left data.
    fn left_semi_join<RV, RM>(
        &self,
        right: &CellMap<K, RV, RM>,
    ) -> CellMap<K, V, CellImmutable>
    where
        RV: CellValue,
        RM: Clone + Send + Sync + 'static;

    /// Left semi join using explicit key extractors.
    ///
    /// `left_key` and `right_key` extract the join key from each side.
    /// Keeps left rows that have at least one matching right row.
    /// Unmatched left rows are excluded. Output contains only left data.
    fn left_semi_join_by<RK, RV, RM, JK, FL, FR>(
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

impl<K, V, M> LeftSemiJoinExt<K, V> for CellMap<K, V, M>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    fn left_semi_join<RV, RM>(
        &self,
        right: &CellMap<K, RV, RM>,
    ) -> CellMap<K, V, CellImmutable>
    where
        RV: CellValue,
        RM: Clone + Send + Sync + 'static,
    {
        run_join_runtime(
            self,
            right,
            "left_semi_join",
            |k: &K, _: &V| k.clone(),
            |k: &K, _: &RV| k.clone(),
            |left_k, left_v, rights| {
                if rights.is_empty() {
                    Vec::new()
                } else {
                    vec![(left_k.clone(), left_v.clone())]
                }
            },
        )
    }

    fn left_semi_join_by<RK, RV, RM, JK, FL, FR>(
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
            "left_semi_join_by",
            left_key,
            right_key,
            |left_k, left_v, rights| {
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
    use crate::{MapDiff, traits::Gettable};

    #[test]
    fn left_semi_join_keeps_matched_rows() {
        let left = CellMap::<String, i32>::new();
        let right = CellMap::<String, bool>::new();
        let joined = left.left_semi_join(&right);

        left.insert("a".to_string(), 1);
        assert_eq!(joined.entries().get().len(), 0);

        right.insert("a".to_string(), true);
        assert_eq!(joined.get_value(&"a".to_string()), Some(1));
    }

    #[test]
    fn left_semi_join_drops_unmatched_rows() {
        let left = CellMap::<String, i32>::new();
        let right = CellMap::<String, bool>::new();
        let joined = left.left_semi_join(&right);

        left.insert("a".to_string(), 1);
        left.insert("b".to_string(), 2);
        right.insert("a".to_string(), true);

        assert_eq!(joined.entries().get().len(), 1);
        assert_eq!(joined.get_value(&"a".to_string()), Some(1));
        assert_eq!(joined.get_value(&"b".to_string()), None);
    }

    #[test]
    fn left_semi_join_reacts_to_right_removal() {
        let left = CellMap::<String, i32>::new();
        let right = CellMap::<String, bool>::new();
        let joined = left.left_semi_join(&right);

        left.insert("a".to_string(), 1);
        right.insert("a".to_string(), true);
        assert_eq!(joined.entries().get().len(), 1);

        right.remove(&"a".to_string());
        assert_eq!(joined.entries().get().len(), 0);
    }

    #[test]
    fn left_semi_join_by_uses_custom_keys() {
        let left = CellMap::<String, i32>::new();
        let right = CellMap::<String, i32>::new();
        let joined = left.left_semi_join_by(
            &right,
            |_, lv| lv.to_string(),
            |rk, _| rk.clone(),
        );

        left.insert("a".to_string(), 10);
        assert_eq!(joined.get_value(&"a".to_string()), None);

        right.insert("10".to_string(), 99);
        assert_eq!(joined.get_value(&"a".to_string()), Some(10));
    }

    #[test]
    fn left_semi_join_by_preserves_right_batch() {
        let left = CellMap::<String, i32>::new();
        left.insert("a".to_string(), 1);
        left.insert("b".to_string(), 2);

        let right = CellMap::<String, bool>::new();
        let joined = left.left_semi_join_by(
            &right,
            |_, lv| lv.to_string(),
            |rk, _| rk.clone(),
        );

        let (tx, rx) = mpsc::channel::<MapDiff<String, i32>>();
        let _guard = joined.subscribe_diffs(move |diff| {
            let _ = tx.send(diff.clone());
        });

        right.insert_many(vec![("1".to_string(), true), ("2".to_string(), true)]);

        let seen: Vec<_> = rx.try_iter().collect();
        assert_eq!(seen.len(), 2);
        match seen.last().expect("last diff") {
            MapDiff::Batch { changes } => assert_eq!(changes.len(), 2),
            _ => panic!("expected batch diff from left_semi_join_by"),
        }
    }
}
