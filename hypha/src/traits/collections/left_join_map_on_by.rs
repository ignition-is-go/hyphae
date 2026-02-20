use std::hash::Hash;

use crate::{
    cell::CellImmutable,
    cell_map::CellMap,
    traits::{CellValue, collections::internal::join_runtime::run_join_runtime},
};

pub trait LeftJoinMapOnByExt<K, V>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    /// Left-joins this map against `lookup` using `lookup_key`, then maps each left row.
    ///
    /// `lookup_key(&left_key, &left_value)` chooses which right row to read.
    /// `mapper(&left_key, &left_value, right_or_none)` always runs once per left row.
    fn left_join_map_on_by<U, LK, R, LM, FK, FM>(
        &self,
        lookup: &CellMap<LK, R, LM>,
        lookup_key: FK,
        mapper: FM,
    ) -> CellMap<K, U, CellImmutable>
    where
        U: CellValue,
        LK: Hash + Eq + CellValue,
        R: CellValue,
        LM: Clone + Send + Sync + 'static,
        FK: Fn(&K, &V) -> LK + Send + Sync + 'static,
        FM: Fn(&K, &V, Option<&R>) -> U + Send + Sync + 'static;
}

impl<K, V, M> LeftJoinMapOnByExt<K, V> for CellMap<K, V, M>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    fn left_join_map_on_by<U, LK, R, LM, FK, FM>(
        &self,
        lookup: &CellMap<LK, R, LM>,
        lookup_key: FK,
        mapper: FM,
    ) -> CellMap<K, U, CellImmutable>
    where
        U: CellValue,
        LK: Hash + Eq + CellValue,
        R: CellValue,
        LM: Clone + Send + Sync + 'static,
        FK: Fn(&K, &V) -> LK + Send + Sync + 'static,
        FM: Fn(&K, &V, Option<&R>) -> U + Send + Sync + 'static,
    {
        run_join_runtime(
            self,
            lookup,
            "left_join_map_on_by",
            lookup_key,
            |lookup_k, _| lookup_k.clone(),
            move |left_k, left_v, rights| {
                let right = rights.first().map(|(_, right_value)| right_value);
                vec![(left_k.clone(), mapper(left_k, left_v, right))]
            },
        )
    }
}
