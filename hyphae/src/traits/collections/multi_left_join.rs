use std::hash::Hash;

use crate::{
    cell::CellImmutable,
    cell_map::CellMap,
    traits::{
        CellValue, collections::internal::multi_join_runtime::run_multi_join_runtime,
        reactive_keys::ReactiveKeys, reactive_map::ReactiveMap,
    },
};

type MultiLeftJoinResult<L, R> = CellMap<
    <L as ReactiveKeys>::Key,
    (<L as ReactiveMap>::Value, Vec<<R as ReactiveMap>::Value>),
    CellImmutable,
>;

pub trait MultiLeftJoinExt: ReactiveMap {
    /// Left join where each left item maps to multiple join keys.
    ///
    /// `left_keys` extracts a `Vec` of join keys from each left item. Right items
    /// matching **any** of those keys are collected into the output `Vec`. Duplicate
    /// right items (reachable via multiple join keys) are deduplicated by right key.
    fn multi_left_join_by<R, JK, FL, FR>(
        &self,
        right: &R,
        left_keys: FL,
        right_key: FR,
    ) -> MultiLeftJoinResult<Self, R>
    where
        R: ReactiveMap,
        JK: Hash + Eq + CellValue,
        FL: Fn(&Self::Key, &Self::Value) -> Vec<JK> + Send + Sync + 'static,
        FR: Fn(&R::Key, &R::Value) -> JK + Send + Sync + 'static;
}

impl<L: ReactiveMap> MultiLeftJoinExt for L {
    fn multi_left_join_by<R, JK, FL, FR>(
        &self,
        right: &R,
        left_keys: FL,
        right_key: FR,
    ) -> MultiLeftJoinResult<Self, R>
    where
        R: ReactiveMap,
        JK: Hash + Eq + CellValue,
        FL: Fn(&Self::Key, &Self::Value) -> Vec<JK> + Send + Sync + 'static,
        FR: Fn(&R::Key, &R::Value) -> JK + Send + Sync + 'static,
    {
        run_multi_join_runtime(
            self,
            right,
            "multi_left_join_by",
            left_keys,
            right_key,
            |left_k, left_v, rights| {
                let right_values: Vec<R::Value> = rights.iter().map(|(_, rv)| rv.clone()).collect();
                vec![(left_k.clone(), (left_v.clone(), right_values))]
            },
        )
    }
}
