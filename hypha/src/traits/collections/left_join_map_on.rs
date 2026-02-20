use std::hash::Hash;

use crate::{
    cell::CellImmutable,
    cell_map::CellMap,
    traits::{
        CellValue, HasForeignKey, IdFor, JoinKeyFrom,
        collections::internal::join_runtime::run_join_runtime,
    },
};

pub trait LeftJoinMapOnExt<K, V>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    /// Left-joins using FK/ID conventions and maps each left row.
    ///
    /// `mapper(&left_key, &left_value, right_or_none)` always runs once per left row.
    fn left_join_map_on<RK, RV, RM, U, FM>(
        &self,
        right: &CellMap<RK, RV, RM>,
        mapper: FM,
    ) -> CellMap<K, U, CellImmutable>
    where
        RK: Hash + Eq + CellValue,
        RV: CellValue,
        U: CellValue,
        V: HasForeignKey<RV>,
        <V as HasForeignKey<RV>>::ForeignKey: IdFor<RV>,
        <<V as HasForeignKey<RV>>::ForeignKey as IdFor<RV>>::MapKey: Hash + Eq + CellValue,
        RK: IdFor<RV>,
        <<V as HasForeignKey<RV>>::ForeignKey as IdFor<RV>>::MapKey:
            JoinKeyFrom<<RK as IdFor<RV>>::MapKey>,
        FM: Fn(&K, &V, Option<&RV>) -> U + Send + Sync + 'static;
}

impl<K, V, M> LeftJoinMapOnExt<K, V> for CellMap<K, V, M>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    fn left_join_map_on<RK, RV, RM, U, FM>(
        &self,
        right: &CellMap<RK, RV, RM>,
        mapper: FM,
    ) -> CellMap<K, U, CellImmutable>
    where
        RK: Hash + Eq + CellValue,
        RV: CellValue,
        U: CellValue,
        V: HasForeignKey<RV>,
        <V as HasForeignKey<RV>>::ForeignKey: IdFor<RV>,
        <<V as HasForeignKey<RV>>::ForeignKey as IdFor<RV>>::MapKey: Hash + Eq + CellValue,
        RK: IdFor<RV>,
        <<V as HasForeignKey<RV>>::ForeignKey as IdFor<RV>>::MapKey:
            JoinKeyFrom<<RK as IdFor<RV>>::MapKey>,
        FM: Fn(&K, &V, Option<&RV>) -> U + Send + Sync + 'static,
    {
        run_join_runtime(
            self,
            right,
            "left_join_map_on",
            |_, left_value| left_value.fk().map_key(),
            |right_key, _| {
                <<V as HasForeignKey<RV>>::ForeignKey as IdFor<RV>>::MapKey::join_key_from(
                    &right_key.map_key(),
                )
            },
            move |left_key, left_value, rights| {
                let right_value = rights.first().map(|(_, right_value)| right_value);
                vec![(left_key.clone(), mapper(left_key, left_value, right_value))]
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn left_join_map_on_uses_fk_defaults() {
        let left = CellMap::<String, (String, i32)>::new();
        let right = CellMap::<String, (String, i32)>::new();
        let joined = left.left_join_map_on(&right, |_, lv, rv| lv.1 + rv.map(|v| v.1).unwrap_or(0));

        left.insert("l1".to_string(), ("g1".to_string(), 10));
        assert_eq!(joined.get_value(&"l1".to_string()), Some(10));
        right.insert("g1".to_string(), ("g1".to_string(), 7));
        assert_eq!(joined.get_value(&"l1".to_string()), Some(17));
    }
}
