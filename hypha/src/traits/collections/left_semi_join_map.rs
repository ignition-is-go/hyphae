use std::hash::Hash;

use super::LeftSemiJoinMapOnByExt;
use crate::{cell::CellImmutable, cell_map::CellMap, traits::CellValue};

pub trait LeftSemiJoinMapExt<K, V>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    /// Keeps left rows with at least one right row on the same map key, then maps those rows.
    fn left_semi_join_map<R, RM, U, FM>(
        &self,
        right: &CellMap<K, R, RM>,
        mapper: FM,
    ) -> CellMap<K, U, CellImmutable>
    where
        R: CellValue,
        RM: Clone + Send + Sync + 'static,
        U: CellValue,
        FM: Fn(&K, &V, &R) -> U + Send + Sync + 'static;
}

impl<K, V, M> LeftSemiJoinMapExt<K, V> for CellMap<K, V, M>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    fn left_semi_join_map<R, RM, U, FM>(
        &self,
        right: &CellMap<K, R, RM>,
        mapper: FM,
    ) -> CellMap<K, U, CellImmutable>
    where
        R: CellValue,
        RM: Clone + Send + Sync + 'static,
        U: CellValue,
        FM: Fn(&K, &V, &R) -> U + Send + Sync + 'static,
    {
        self.left_semi_join_map_on_by(
            right,
            |left_key, _| left_key.clone(),
            |right_key, _| right_key.clone(),
            mapper,
        )
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;
    use crate::MapDiff;

    #[test]
    fn left_semi_join_map_filters_and_maps_on_equal_keys() {
        let left = CellMap::<String, i32>::new();
        let right = CellMap::<String, i32>::new();
        let joined = left.left_semi_join_map(&right, |_, lv, rv| lv + rv);

        left.insert("a".to_string(), 2);
        assert_eq!(joined.get_value(&"a".to_string()), None);

        right.insert("a".to_string(), 3);
        assert_eq!(joined.get_value(&"a".to_string()), Some(5));

        right.remove(&"a".to_string());
        assert_eq!(joined.get_value(&"a".to_string()), None);
    }

    #[test]
    fn left_semi_join_map_right_batch_emits_single_batch() {
        let left = CellMap::<String, i32>::new();
        let right = CellMap::<String, i32>::new();
        left.insert("a".to_string(), 1);
        left.insert("b".to_string(), 2);
        let out = left.left_semi_join_map(&right, |_, lv, rv| lv + rv);

        let (tx, rx) = mpsc::channel::<MapDiff<String, i32>>();
        let _guard = out.subscribe_diffs(move |diff| {
            let _ = tx.send(diff.clone());
        });

        right.insert_many(vec![("a".to_string(), 10), ("b".to_string(), 20)]);

        let seen: Vec<_> = rx.try_iter().collect();
        assert_eq!(seen.len(), 2);
        match seen.last().expect("last diff") {
            MapDiff::Batch { changes } => assert_eq!(changes.len(), 2),
            _ => panic!("expected batch diff from left_semi_join_map"),
        }
    }
}
