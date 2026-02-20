use std::hash::Hash;

use super::LeftJoinMapOnByExt;
use crate::{cell::CellImmutable, cell_map::CellMap, traits::CellValue};

pub trait LeftJoinMapExt<K, V>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    /// Left-joins on equal map keys and maps each left row.
    ///
    /// Equivalent to `left_join_map_on_by(right, |left_key, _| left_key.clone(), mapper)`.
    fn left_join_map<R, RM, U, FM>(
        &self,
        right: &CellMap<K, R, RM>,
        mapper: FM,
    ) -> CellMap<K, U, CellImmutable>
    where
        R: CellValue,
        RM: Clone + Send + Sync + 'static,
        U: CellValue,
        FM: Fn(&K, &V, Option<&R>) -> U + Send + Sync + 'static;
}

impl<K, V, M> LeftJoinMapExt<K, V> for CellMap<K, V, M>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    fn left_join_map<R, RM, U, FM>(
        &self,
        right: &CellMap<K, R, RM>,
        mapper: FM,
    ) -> CellMap<K, U, CellImmutable>
    where
        R: CellValue,
        RM: Clone + Send + Sync + 'static,
        U: CellValue,
        FM: Fn(&K, &V, Option<&R>) -> U + Send + Sync + 'static,
    {
        self.left_join_map_on_by(right, |left_key, _| left_key.clone(), mapper)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn left_join_map_joins_on_equal_map_keys() {
        let left = CellMap::<String, i32>::new();
        let right = CellMap::<String, i32>::new();
        let joined = left.left_join_map(&right, |_, lv, rv| lv + rv.copied().unwrap_or(0));

        left.insert("a".to_string(), 2);
        assert_eq!(joined.get_value(&"a".to_string()), Some(2));
        right.insert("a".to_string(), 3);
        assert_eq!(joined.get_value(&"a".to_string()), Some(5));
    }
}
