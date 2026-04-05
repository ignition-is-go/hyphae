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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::Gettable;

    #[test]
    fn multi_join_empty_keys_produces_empty_right_vec() {
        let left = CellMap::<String, i32>::new();
        let right = CellMap::<String, i32>::new();
        let joined =
            left.multi_left_join_by(&right, |_k, _v| Vec::<String>::new(), |k, _v| k.clone());

        left.insert("l1".to_string(), 1);
        let val = joined.get_value(&"l1".to_string());
        assert_eq!(val, Some((1, vec![])));
    }

    #[test]
    fn multi_join_single_key_matches_like_left_join() {
        let left = CellMap::<String, String>::new();
        let right = CellMap::<String, (String, i32)>::new();
        let joined = left.multi_left_join_by(&right, |_k, v| vec![v.clone()], |_k, v| v.0.clone());

        left.insert("l1".to_string(), "g1".to_string());
        right.insert("r1".to_string(), ("g1".to_string(), 10));

        let (left_val, right_vals) = joined.get_value(&"l1".to_string()).unwrap();
        assert_eq!(left_val, "g1");
        assert_eq!(right_vals.len(), 1);
        assert_eq!(right_vals[0].0, "g1");
    }

    #[test]
    fn multi_join_multiple_keys_collects_from_all() {
        let left = CellMap::<String, Vec<String>>::new();
        let right = CellMap::<String, (String, i32)>::new();
        let joined = left.multi_left_join_by(&right, |_k, v| v.clone(), |_k, v| v.0.clone());

        left.insert("l1".to_string(), vec!["g1".to_string(), "g2".to_string()]);
        right.insert("r1".to_string(), ("g1".to_string(), 10));
        right.insert("r2".to_string(), ("g2".to_string(), 20));
        right.insert("r3".to_string(), ("g3".to_string(), 30));

        let (_, right_vals) = joined.get_value(&"l1".to_string()).unwrap();
        assert_eq!(right_vals.len(), 2);
    }

    #[test]
    fn multi_join_deduplicates_right_items_across_keys() {
        let left = CellMap::<String, Vec<String>>::new();
        let right = CellMap::<String, (String, i32)>::new();
        let joined = left.multi_left_join_by(&right, |_k, v| v.clone(), |_k, v| v.0.clone());

        left.insert("l1".to_string(), vec!["g1".to_string(), "g1".to_string()]);
        right.insert("r1".to_string(), ("g1".to_string(), 10));

        let (_, right_vals) = joined.get_value(&"l1".to_string()).unwrap();
        assert_eq!(right_vals.len(), 1);
    }

    #[test]
    fn multi_join_reacts_to_right_addition() {
        let left = CellMap::<String, Vec<String>>::new();
        let right = CellMap::<String, (String, i32)>::new();
        let joined = left.multi_left_join_by(&right, |_k, v| v.clone(), |_k, v| v.0.clone());

        left.insert("l1".to_string(), vec!["g1".to_string(), "g2".to_string()]);
        assert_eq!(joined.get_value(&"l1".to_string()).unwrap().1.len(), 0);

        right.insert("r1".to_string(), ("g1".to_string(), 10));
        assert_eq!(joined.get_value(&"l1".to_string()).unwrap().1.len(), 1);

        right.insert("r2".to_string(), ("g2".to_string(), 20));
        assert_eq!(joined.get_value(&"l1".to_string()).unwrap().1.len(), 2);
    }

    #[test]
    fn multi_join_reacts_to_right_removal() {
        let left = CellMap::<String, Vec<String>>::new();
        let right = CellMap::<String, (String, i32)>::new();
        let joined = left.multi_left_join_by(&right, |_k, v| v.clone(), |_k, v| v.0.clone());

        left.insert("l1".to_string(), vec!["g1".to_string()]);
        right.insert("r1".to_string(), ("g1".to_string(), 10));
        assert_eq!(joined.get_value(&"l1".to_string()).unwrap().1.len(), 1);

        right.remove(&"r1".to_string());
        assert_eq!(joined.get_value(&"l1".to_string()).unwrap().1.len(), 0);
    }

    #[test]
    fn multi_join_reacts_to_left_key_change() {
        let left = CellMap::<String, Vec<String>>::new();
        let right = CellMap::<String, (String, i32)>::new();
        let joined = left.multi_left_join_by(&right, |_k, v| v.clone(), |_k, v| v.0.clone());

        right.insert("r1".to_string(), ("g1".to_string(), 10));
        right.insert("r2".to_string(), ("g2".to_string(), 20));

        left.insert("l1".to_string(), vec!["g1".to_string()]);
        assert_eq!(joined.get_value(&"l1".to_string()).unwrap().1.len(), 1);

        left.insert("l1".to_string(), vec!["g2".to_string()]);
        let (_, rights) = joined.get_value(&"l1".to_string()).unwrap();
        assert_eq!(rights.len(), 1);
        assert_eq!(rights[0].0, "g2");
    }

    #[test]
    fn multi_join_reacts_to_left_removal() {
        let left = CellMap::<String, Vec<String>>::new();
        let right = CellMap::<String, (String, i32)>::new();
        let joined = left.multi_left_join_by(&right, |_k, v| v.clone(), |_k, v| v.0.clone());

        left.insert("l1".to_string(), vec!["g1".to_string()]);
        right.insert("r1".to_string(), ("g1".to_string(), 10));
        assert_eq!(joined.entries().get().len(), 1);

        left.remove(&"l1".to_string());
        assert_eq!(joined.entries().get().len(), 0);
    }
}
