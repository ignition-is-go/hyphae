//! Multi-left-join plan node implementing [`MapQuery`].
//!
//! `multi_left_join_by` builds an uncompiled plan node that composes with
//! other [`MapQuery`] operators. Call [`MapQuery::materialize`] to compile a
//! plan into a subscribable [`CellMap`](crate::CellMap).

use std::{hash::Hash, marker::PhantomData};

use crate::{
    map_query::{MapDiffSink, MapQuery, MapQueryInstall},
    subscription::SubscriptionGuard,
    traits::{
        CellValue,
        collections::internal::multi_join_runtime::install_multi_join_runtime_via_query,
    },
};

/// Plan node for [`MultiLeftJoinExt::multi_left_join_by`].
///
/// Each left row produces exactly one output row, keyed by the left key.
/// Each left item maps to multiple join keys via `left_keys`; right rows
/// matching **any** of those keys are collected into the output `Vec`.
/// Duplicate right items (reachable via multiple join keys) are deduplicated
/// by right key. Output value type is `(LV, Vec<RV>)`.
///
/// Not [`Clone`]: cloning a plan would silently duplicate join work; share by
/// materializing once.
pub struct MultiLeftJoinPlan<L, R, LK, LV, RK, RV, JK, FL, FR>
where
    L: MapQuery<LK, LV>,
    R: MapQuery<RK, RV>,
    LK: Hash + Eq + CellValue,
    LV: CellValue,
    RK: Hash + Eq + CellValue,
    RV: CellValue,
    JK: Hash + Eq + CellValue,
    FL: Fn(&LK, &LV) -> Vec<JK> + Send + Sync + 'static,
    FR: Fn(&RK, &RV) -> JK + Send + Sync + 'static,
{
    pub(crate) left: L,
    pub(crate) right: R,
    pub(crate) left_keys: FL,
    pub(crate) right_key: FR,
    #[allow(clippy::type_complexity)]
    pub(crate) _types: PhantomData<fn() -> (LK, LV, RK, RV, JK)>,
}

impl<L, R, LK, LV, RK, RV, JK, FL, FR> MapQueryInstall<LK, (LV, Vec<RV>)>
    for MultiLeftJoinPlan<L, R, LK, LV, RK, RV, JK, FL, FR>
where
    L: MapQuery<LK, LV>,
    R: MapQuery<RK, RV>,
    LK: Hash + Eq + CellValue,
    LV: CellValue,
    RK: Hash + Eq + CellValue,
    RV: CellValue,
    JK: Hash + Eq + CellValue,
    FL: Fn(&LK, &LV) -> Vec<JK> + Send + Sync + 'static,
    FR: Fn(&RK, &RV) -> JK + Send + Sync + 'static,
{
    fn install(self, sink: MapDiffSink<LK, (LV, Vec<RV>)>) -> Vec<SubscriptionGuard> {
        install_multi_join_runtime_via_query::<LK, LV, RK, RV, JK, LK, (LV, Vec<RV>), _, _, _, _, _>(
            self.left,
            self.right,
            self.left_keys,
            self.right_key,
            |left_k: &LK, left_v: &LV, rights: &[(RK, RV)]| {
                let right_values: Vec<RV> = rights.iter().map(|(_, rv)| rv.clone()).collect();
                vec![(left_k.clone(), (left_v.clone(), right_values))]
            },
            sink,
        )
    }
}

#[allow(private_bounds)]
impl<L, R, LK, LV, RK, RV, JK, FL, FR> MapQuery<LK, (LV, Vec<RV>)>
    for MultiLeftJoinPlan<L, R, LK, LV, RK, RV, JK, FL, FR>
where
    L: MapQuery<LK, LV>,
    R: MapQuery<RK, RV>,
    LK: Hash + Eq + CellValue,
    LV: CellValue,
    RK: Hash + Eq + CellValue,
    RV: CellValue,
    JK: Hash + Eq + CellValue,
    FL: Fn(&LK, &LV) -> Vec<JK> + Send + Sync + 'static,
    FR: Fn(&RK, &RV) -> JK + Send + Sync + 'static,
{
}

/// Multi-left-join operator returning a [`MapQuery`] plan node.
///
/// Consumes `self` and returns an uncompiled plan node; call
/// [`MapQuery::materialize`] on the result to obtain a subscribable
/// [`CellMap`](crate::CellMap).
pub trait MultiLeftJoinExt<K, V>: MapQuery<K, V>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    /// Left join where each left item maps to multiple join keys.
    ///
    /// `left_keys` extracts a `Vec` of join keys from each left item. Right
    /// items matching **any** of those keys are collected into the output
    /// `Vec`. Duplicate right items (reachable via multiple join keys) are
    /// deduplicated by right key. Every left row produces exactly one output
    /// row, keyed by the left key.
    fn multi_left_join_by<R, RK, RV, JK, FL, FR>(
        self,
        right: R,
        left_keys: FL,
        right_key: FR,
    ) -> MultiLeftJoinPlan<Self, R, K, V, RK, RV, JK, FL, FR>
    where
        R: MapQuery<RK, RV>,
        RK: Hash + Eq + CellValue,
        RV: CellValue,
        JK: Hash + Eq + CellValue,
        FL: Fn(&K, &V) -> Vec<JK> + Send + Sync + 'static,
        FR: Fn(&RK, &RV) -> JK + Send + Sync + 'static,
    {
        MultiLeftJoinPlan {
            left: self,
            right,
            left_keys,
            right_key,
            _types: PhantomData,
        }
    }
}

impl<K, V, M> MultiLeftJoinExt<K, V> for M
where
    K: Hash + Eq + CellValue,
    V: CellValue,
    M: MapQuery<K, V>,
{
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CellMap, traits::Gettable};

    #[test]
    fn multi_join_empty_keys_produces_empty_right_vec() {
        let left = CellMap::<String, i32>::new();
        let right = CellMap::<String, i32>::new();
        let joined = left
            .clone()
            .multi_left_join_by(right.clone(), |_k, _v| Vec::<String>::new(), |k, _v| k.clone())
            .materialize();

        left.insert("l1".to_string(), 1);
        let val = joined.get_value(&"l1".to_string());
        assert_eq!(val, Some((1, vec![])));
    }

    #[test]
    fn multi_join_single_key_matches_like_left_join() {
        let left = CellMap::<String, String>::new();
        let right = CellMap::<String, (String, i32)>::new();
        let joined = left
            .clone()
            .multi_left_join_by(right.clone(), |_k, v| vec![v.clone()], |_k, v| v.0.clone())
            .materialize();

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
        let joined = left
            .clone()
            .multi_left_join_by(right.clone(), |_k, v| v.clone(), |_k, v| v.0.clone())
            .materialize();

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
        let joined = left
            .clone()
            .multi_left_join_by(right.clone(), |_k, v| v.clone(), |_k, v| v.0.clone())
            .materialize();

        left.insert("l1".to_string(), vec!["g1".to_string(), "g1".to_string()]);
        right.insert("r1".to_string(), ("g1".to_string(), 10));

        let (_, right_vals) = joined.get_value(&"l1".to_string()).unwrap();
        assert_eq!(right_vals.len(), 1);
    }

    #[test]
    fn multi_join_reacts_to_right_addition() {
        let left = CellMap::<String, Vec<String>>::new();
        let right = CellMap::<String, (String, i32)>::new();
        let joined = left
            .clone()
            .multi_left_join_by(right.clone(), |_k, v| v.clone(), |_k, v| v.0.clone())
            .materialize();

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
        let joined = left
            .clone()
            .multi_left_join_by(right.clone(), |_k, v| v.clone(), |_k, v| v.0.clone())
            .materialize();

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
        let joined = left
            .clone()
            .multi_left_join_by(right.clone(), |_k, v| v.clone(), |_k, v| v.0.clone())
            .materialize();

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
        let joined = left
            .clone()
            .multi_left_join_by(right.clone(), |_k, v| v.clone(), |_k, v| v.0.clone())
            .materialize();

        left.insert("l1".to_string(), vec!["g1".to_string()]);
        right.insert("r1".to_string(), ("g1".to_string(), 10));
        assert_eq!(joined.entries().get().len(), 1);

        left.remove(&"l1".to_string());
        assert_eq!(joined.entries().get().len(), 0);
    }
}
