use std::hash::Hash;

use crate::{
    cell::CellImmutable,
    cell_map::CellMap,
    traits::{
        CellValue, HasForeignKey, IdFor, JoinKeyFrom,
        collections::internal::join_runtime::run_join_runtime,
    },
};

pub trait LeftJoinOnExt<K, V>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    /// Left-joins using FK/ID conventions and keeps left rows where `predicate` is true.
    ///
    /// `predicate(&left_key, &left_value, right_or_none)` receives `None` when there is no match.
    fn left_join_on<RK, RV, RM, FP>(
        &self,
        right: &CellMap<RK, RV, RM>,
        predicate: FP,
    ) -> CellMap<K, V, CellImmutable>
    where
        RK: Hash + Eq + CellValue,
        RV: CellValue,
        V: HasForeignKey<RV>,
        <V as HasForeignKey<RV>>::ForeignKey: IdFor<RV>,
        <<V as HasForeignKey<RV>>::ForeignKey as IdFor<RV>>::MapKey: Hash + Eq + CellValue,
        RK: IdFor<RV>,
        <<V as HasForeignKey<RV>>::ForeignKey as IdFor<RV>>::MapKey:
            JoinKeyFrom<<RK as IdFor<RV>>::MapKey>,
        FP: Fn(&K, &V, Option<&RV>) -> bool + Send + Sync + 'static;
}

impl<K, V, M> LeftJoinOnExt<K, V> for CellMap<K, V, M>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    fn left_join_on<RK, RV, RM, FP>(
        &self,
        right: &CellMap<RK, RV, RM>,
        predicate: FP,
    ) -> CellMap<K, V, CellImmutable>
    where
        RK: Hash + Eq + CellValue,
        RV: CellValue,
        V: HasForeignKey<RV>,
        <V as HasForeignKey<RV>>::ForeignKey: IdFor<RV>,
        <<V as HasForeignKey<RV>>::ForeignKey as IdFor<RV>>::MapKey: Hash + Eq + CellValue,
        RK: IdFor<RV>,
        <<V as HasForeignKey<RV>>::ForeignKey as IdFor<RV>>::MapKey:
            JoinKeyFrom<<RK as IdFor<RV>>::MapKey>,
        FP: Fn(&K, &V, Option<&RV>) -> bool + Send + Sync + 'static,
    {
        run_join_runtime(
            self,
            right,
            "left_join_on",
            |_, left_value| left_value.fk().map_key(),
            |right_key, _| {
                <<V as HasForeignKey<RV>>::ForeignKey as IdFor<RV>>::MapKey::join_key_from(
                    &right_key.map_key(),
                )
            },
            move |left_key, left_value, rights| {
                let right_value = rights.first().map(|(_, right_value)| right_value);
                if predicate(left_key, left_value, right_value) {
                    vec![(left_key.clone(), left_value.clone())]
                } else {
                    Vec::new()
                }
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::Gettable;

    #[test]
    fn left_join_on_uses_fk_defaults() {
        let left = CellMap::<String, (String, i32)>::new();
        let right = CellMap::<String, (String, i32)>::new();
        let joined = left.left_join_on(&right, |_, _, rv| rv.is_some());

        left.insert("l1".to_string(), ("g1".to_string(), 10));
        assert_eq!(joined.entries().get().len(), 0);
        right.insert("g1".to_string(), ("g1".to_string(), 7));
        assert_eq!(joined.entries().get().len(), 1);
    }
}
