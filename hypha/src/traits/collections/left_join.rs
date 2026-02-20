use std::hash::Hash;

use super::LeftJoinOnByExt;
use crate::{cell::CellImmutable, cell_map::CellMap, traits::CellValue};

pub trait LeftJoinExt<K, V>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    /// Left-joins on equal map keys and keeps rows where `predicate` is true.
    ///
    /// Equivalent to `left_join_on_by(right, |left_key, _| left_key.clone(), predicate)`.
    fn left_join<R, RM, FP>(
        &self,
        right: &CellMap<K, R, RM>,
        predicate: FP,
    ) -> CellMap<K, V, CellImmutable>
    where
        R: CellValue,
        RM: Clone + Send + Sync + 'static,
        FP: Fn(&K, &V, Option<&R>) -> bool + Send + Sync + 'static;
}

impl<K, V, M> LeftJoinExt<K, V> for CellMap<K, V, M>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    fn left_join<R, RM, FP>(
        &self,
        right: &CellMap<K, R, RM>,
        predicate: FP,
    ) -> CellMap<K, V, CellImmutable>
    where
        R: CellValue,
        RM: Clone + Send + Sync + 'static,
        FP: Fn(&K, &V, Option<&R>) -> bool + Send + Sync + 'static,
    {
        self.left_join_on_by(right, |left_key, _| left_key.clone(), predicate)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::Gettable;

    #[test]
    fn left_join_filters_on_equal_map_keys() {
        let left = CellMap::<String, i32>::new();
        let right = CellMap::<String, bool>::new();
        let joined = left.left_join(&right, |_, _, rv| rv.copied().unwrap_or(false));

        left.insert("a".to_string(), 1);
        assert_eq!(joined.entries().get().len(), 0);
        right.insert("a".to_string(), true);
        assert_eq!(joined.entries().get().len(), 1);
    }
}
