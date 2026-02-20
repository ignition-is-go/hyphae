use std::hash::Hash;

use crate::{
    cell::CellImmutable,
    cell_map::CellMap,
    traits::{CellValue, collections::internal::diff_runtime::run_refcount_runtime},
};

pub trait DistinctMapExt<K, V>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    /// Keeps at most one visible row per key from this map.
    ///
    /// Repeated inserts for the same key increase an internal refcount; the row is removed only
    /// when that key's refcount drops back to zero.
    #[track_caller]
    fn distinct(&self) -> CellMap<K, V, CellImmutable>;
}

impl<K, V, M> DistinctMapExt<K, V> for CellMap<K, V, M>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    fn distinct(&self) -> CellMap<K, V, CellImmutable> {
        run_refcount_runtime(self, "distinct")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MapDiff;

    #[test]
    fn distinct_refcount_transitions_without_extra_side_emissions() {
        let source = CellMap::<String, i32>::new();
        let distinct = source.distinct();

        source.apply_batch(vec![
            MapDiff::Insert {
                key: "a".to_string(),
                value: 1,
            },
            MapDiff::Insert {
                key: "a".to_string(),
                value: 1,
            },
        ]);
        assert_eq!(distinct.get_value(&"a".to_string()), Some(1));

        source.apply_batch(vec![MapDiff::Remove {
            key: "a".to_string(),
            old_value: 1,
        }]);
        assert_eq!(distinct.get_value(&"a".to_string()), Some(1));

        source.apply_batch(vec![MapDiff::Remove {
            key: "a".to_string(),
            old_value: 1,
        }]);
        assert_eq!(distinct.get_value(&"a".to_string()), None);
    }
}
