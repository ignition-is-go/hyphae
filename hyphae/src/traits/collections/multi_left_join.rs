//! Multi-left-join plan node implementing [`MapQuery`].
//!
//! `multi_left_join_by` builds an uncompiled plan node that composes with
//! other [`MapQuery`] operators. Call [`MapQuery::materialize`] to compile a
//! plan into a subscribable [`CellMap`](crate::CellMap).

use std::{hash::Hash, marker::PhantomData};

use crate::{
    map_query::{
        MapDiffSink, MapQuery, MapQueryInstall,
        properties::{ByMapKey, ExactlyOne, PlanProperties},
    },
    subscription::SubscriptionGuard,
    traits::{
        CellValue,
        collections::internal::multi_join_runtime::install_keyed_multi_join_runtime_via_query,
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
    L: MapQuery<Key = LK, Value = LV>,
    R: MapQuery<Key = RK, Value = RV>,
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

impl<L, R, LK, LV, RK, RV, JK, FL, FR> PlanProperties
    for MultiLeftJoinPlan<L, R, LK, LV, RK, RV, JK, FL, FR>
where
    L: MapQuery<Key = LK, Value = LV>,
    R: MapQuery<Key = RK, Value = RV>,
    LK: Hash + Eq + CellValue,
    LV: CellValue,
    RK: Hash + Eq + CellValue,
    RV: CellValue,
    JK: Hash + Eq + CellValue,
    FL: Fn(&LK, &LV) -> Vec<JK> + Send + Sync + 'static,
    FR: Fn(&RK, &RV) -> JK + Send + Sync + 'static,
{
    type Cardinality = ExactlyOne;
    type InputPartition = L::OutputPartition;
    type OutputPartition = ByMapKey<LK>;
}

impl<L, R, LK, LV, RK, RV, JK, FL, FR> MapQueryInstall<LK, (LV, Vec<RV>)>
    for MultiLeftJoinPlan<L, R, LK, LV, RK, RV, JK, FL, FR>
where
    L: MapQuery<Key = LK, Value = LV>,
    R: MapQuery<Key = RK, Value = RV>,
    LK: Hash + Eq + CellValue,
    LV: CellValue,
    RK: Hash + Eq + CellValue,
    RV: CellValue,
    JK: Hash + Eq + CellValue,
    FL: Fn(&LK, &LV) -> Vec<JK> + Send + Sync + 'static,
    FR: Fn(&RK, &RV) -> JK + Send + Sync + 'static,
{
    fn install<Sink>(
        self,
        cx: &mut crate::map_query::compiler::CompileContext,
        sink: Sink,
    ) -> Vec<SubscriptionGuard>
    where
        Sink: MapDiffSink<LK, (LV, Vec<RV>)>,
    {
        install_keyed_multi_join_runtime_via_query::<
            LK,
            LV,
            RK,
            RV,
            JK,
            (LV, Vec<RV>),
            _,
            _,
            _,
            _,
            _,
            _,
        >(
            cx,
            self.left,
            self.right,
            self.left_keys,
            self.right_key,
            |_left_k: &LK, left_v: &LV, rights: &[(RK, RV)]| {
                let right_values: Vec<RV> = rights.iter().map(|(_, rv)| rv.clone()).collect();
                (left_v.clone(), right_values)
            },
            sink,
        )
    }
}

#[allow(private_bounds)]
impl<L, R, LK, LV, RK, RV, JK, FL, FR> MapQuery
    for MultiLeftJoinPlan<L, R, LK, LV, RK, RV, JK, FL, FR>
where
    L: MapQuery<Key = LK, Value = LV>,
    R: MapQuery<Key = RK, Value = RV>,
    LK: Hash + Eq + CellValue,
    LV: CellValue,
    RK: Hash + Eq + CellValue,
    RV: CellValue,
    JK: Hash + Eq + CellValue,
    FL: Fn(&LK, &LV) -> Vec<JK> + Send + Sync + 'static,
    FR: Fn(&RK, &RV) -> JK + Send + Sync + 'static,
{
    type Key = LK;
    type Value = (LV, Vec<RV>);
}

/// Multi-left-join operator returning a [`MapQuery`] plan node.
///
/// Consumes `self` and returns an uncompiled plan node; call
/// [`MapQuery::materialize`] on the result to obtain a subscribable
/// [`CellMap`](crate::CellMap).
pub trait MultiLeftJoinExt<K, V>: MapQuery<Key = K, Value = V>
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
    ) -> impl MapQuery<Key = K, Value = (V, Vec<RV>)>
    where
        R: MapQuery<Key = RK, Value = RV>,
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
    M: MapQuery<Key = K, Value = V>,
{
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CellMap, Materialize, traits::Gettable};

    #[test]
    fn multi_join_empty_keys_produces_empty_right_vec() {
        let left = CellMap::<String, i32>::new();
        let right = CellMap::<String, i32>::new();
        let joined = left
            .clone()
            .multi_left_join_by(right, |_k, _v| Vec::<String>::new(), |k, _v| k.clone())
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

        assert!(matches!(
            joined.get_value(&"l1".to_string()),
            Some((left_val, right_vals))
                if left_val == "g1"
                    && right_vals.len() == 1
                    && right_vals.first().is_some_and(|right| right.0 == "g1")
        ));
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

        assert!(matches!(
            joined.get_value(&"l1".to_string()),
            Some((_, right_vals)) if right_vals.len() == 2
        ));
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

        assert!(matches!(
            joined.get_value(&"l1".to_string()),
            Some((_, right_vals)) if right_vals.len() == 1
        ));
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
        assert!(matches!(
            joined.get_value(&"l1".to_string()),
            Some((_, rights)) if rights.is_empty()
        ));

        right.insert("r1".to_string(), ("g1".to_string(), 10));
        assert!(matches!(
            joined.get_value(&"l1".to_string()),
            Some((_, rights)) if rights.len() == 1
        ));

        right.insert("r2".to_string(), ("g2".to_string(), 20));
        assert!(matches!(
            joined.get_value(&"l1".to_string()),
            Some((_, rights)) if rights.len() == 2
        ));
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
        assert!(matches!(
            joined.get_value(&"l1".to_string()),
            Some((_, rights)) if rights.len() == 1
        ));

        right.remove(&"r1".to_string());
        assert!(matches!(
            joined.get_value(&"l1".to_string()),
            Some((_, rights)) if rights.is_empty()
        ));
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
        assert!(matches!(
            joined.get_value(&"l1".to_string()),
            Some((_, rights)) if rights.len() == 1
        ));

        left.insert("l1".to_string(), vec!["g2".to_string()]);
        assert!(matches!(
            joined.get_value(&"l1".to_string()),
            Some((_, rights))
                if rights.len() == 1
                    && rights.first().is_some_and(|right| right.0 == "g2")
        ));
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
        assert_eq!(joined.entries().materialize().get().len(), 1);

        left.remove(&"l1".to_string());
        assert_eq!(joined.entries().materialize().get().len(), 0);
    }
}
