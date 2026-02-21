use std::hash::Hash;

use crate::{
    cell::CellImmutable,
    cell_map::CellMap,
    traits::{CellValue, collections::internal::join_runtime::run_join_runtime},
};

pub trait LeftJoinExt<K, V>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    /// Left join on equal map keys.
    ///
    /// Every left row produces exactly one output row. Right matches are collected into a `Vec`.
    /// An empty `Vec` means no matching right rows were found.
    fn left_join<RV, RM>(
        &self,
        right: &CellMap<K, RV, RM>,
    ) -> CellMap<K, (V, Vec<RV>), CellImmutable>
    where
        RV: CellValue,
        RM: Clone + Send + Sync + 'static;

    /// Left join using explicit key extractors.
    ///
    /// `left_key` and `right_key` extract the join key from each side.
    /// Every left row produces exactly one output row. Right matches are collected into a `Vec`.
    /// An empty `Vec` means no matching right rows were found.
    fn left_join_by<RK, RV, RM, JK, FL, FR>(
        &self,
        right: &CellMap<RK, RV, RM>,
        left_key: FL,
        right_key: FR,
    ) -> CellMap<K, (V, Vec<RV>), CellImmutable>
    where
        RK: Hash + Eq + CellValue,
        RV: CellValue,
        JK: Hash + Eq + CellValue,
        FL: Fn(&K, &V) -> JK + Send + Sync + 'static,
        FR: Fn(&RK, &RV) -> JK + Send + Sync + 'static;
}

impl<K, V, M> LeftJoinExt<K, V> for CellMap<K, V, M>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    fn left_join<RV, RM>(
        &self,
        right: &CellMap<K, RV, RM>,
    ) -> CellMap<K, (V, Vec<RV>), CellImmutable>
    where
        RV: CellValue,
        RM: Clone + Send + Sync + 'static,
    {
        run_join_runtime(
            self,
            right,
            "left_join",
            |k: &K, _: &V| k.clone(),
            |k: &K, _: &RV| k.clone(),
            |left_k, left_v, rights| {
                let right_values: Vec<RV> =
                    rights.iter().map(|(_, rv)| rv.clone()).collect();
                vec![(left_k.clone(), (left_v.clone(), right_values))]
            },
        )
    }

    fn left_join_by<RK, RV, RM, JK, FL, FR>(
        &self,
        right: &CellMap<RK, RV, RM>,
        left_key: FL,
        right_key: FR,
    ) -> CellMap<K, (V, Vec<RV>), CellImmutable>
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
            "left_join_by",
            left_key,
            right_key,
            |left_k, left_v, rights| {
                let right_values: Vec<RV> =
                    rights.iter().map(|(_, rv)| rv.clone()).collect();
                vec![(left_k.clone(), (left_v.clone(), right_values))]
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
    fn left_join_keeps_unmatched_left_rows() {
        let left = CellMap::<String, i32>::new();
        let right = CellMap::<String, i32>::new();
        let joined = left.left_join(&right);

        left.insert("a".to_string(), 1);
        assert_eq!(joined.get_value(&"a".to_string()), Some((1, vec![])));
    }

    #[test]
    fn left_join_pairs_matched_rows() {
        let left = CellMap::<String, i32>::new();
        let right = CellMap::<String, i32>::new();
        let joined = left.left_join(&right);

        left.insert("a".to_string(), 1);
        right.insert("a".to_string(), 10);
        assert_eq!(joined.get_value(&"a".to_string()), Some((1, vec![10])));
    }

    #[test]
    fn left_join_reacts_to_right_addition() {
        let left = CellMap::<String, i32>::new();
        let right = CellMap::<String, i32>::new();
        let joined = left.left_join(&right);

        left.insert("a".to_string(), 1);
        assert_eq!(joined.get_value(&"a".to_string()), Some((1, vec![])));

        right.insert("a".to_string(), 10);
        assert_eq!(joined.get_value(&"a".to_string()), Some((1, vec![10])));
    }

    #[test]
    fn left_join_reacts_to_right_removal() {
        let left = CellMap::<String, i32>::new();
        let right = CellMap::<String, i32>::new();
        let joined = left.left_join(&right);

        left.insert("a".to_string(), 1);
        right.insert("a".to_string(), 10);
        assert_eq!(joined.get_value(&"a".to_string()), Some((1, vec![10])));

        right.remove(&"a".to_string());
        assert_eq!(joined.get_value(&"a".to_string()), Some((1, vec![])));
    }

    #[test]
    fn left_join_reacts_to_left_removal() {
        let left = CellMap::<String, i32>::new();
        let right = CellMap::<String, i32>::new();
        let joined = left.left_join(&right);

        left.insert("a".to_string(), 1);
        right.insert("a".to_string(), 10);
        assert_eq!(joined.entries().get().len(), 1);

        left.remove(&"a".to_string());
        assert_eq!(joined.entries().get().len(), 0);
    }

    #[test]
    fn left_join_by_collects_multiple_right_matches() {
        let left = CellMap::<String, (String, i32)>::new();
        let right = CellMap::<String, (String, i32)>::new();
        let joined = left.left_join_by(
            &right,
            |_, lv| lv.0.clone(),
            |_, rv| rv.0.clone(),
        );

        left.insert("l1".to_string(), ("g1".to_string(), 10));
        right.insert("r1".to_string(), ("g1".to_string(), 5));
        right.insert("r2".to_string(), ("g1".to_string(), 7));

        let val = joined.get_value(&"l1".to_string());
        assert!(val.is_some());
        let (left_val, right_vals) = val.unwrap();
        assert_eq!(left_val, ("g1".to_string(), 10));
        assert_eq!(right_vals.len(), 2);
    }

    #[test]
    fn left_join_by_keeps_unmatched_with_empty_vec() {
        let left = CellMap::<String, (String, i32)>::new();
        let right = CellMap::<String, (String, i32)>::new();
        let joined = left.left_join_by(
            &right,
            |_, lv| lv.0.clone(),
            |_, rv| rv.0.clone(),
        );

        left.insert("l1".to_string(), ("g1".to_string(), 10));

        let val = joined.get_value(&"l1".to_string());
        assert!(val.is_some());
        let (left_val, right_vals) = val.unwrap();
        assert_eq!(left_val, ("g1".to_string(), 10));
        assert_eq!(right_vals.len(), 0);
    }

    #[test]
    fn left_join_by_preserves_right_batch() {
        let left = CellMap::<String, (String, i32)>::new();
        left.insert("l1".to_string(), ("g1".to_string(), 10));

        let right = CellMap::<String, (String, i32)>::new();
        let joined = left.left_join_by(
            &right,
            |_, lv| lv.0.clone(),
            |_, rv| rv.0.clone(),
        );

        let (tx, rx) =
            mpsc::channel::<MapDiff<String, ((String, i32), Vec<(String, i32)>)>>();
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
            MapDiff::Batch { changes } => assert!(!changes.is_empty()),
            _ => panic!("expected batch diff from left_join_by"),
        }
    }
}
