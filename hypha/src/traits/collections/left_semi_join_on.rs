use std::hash::Hash;

use super::LeftSemiJoinOnByExt;
use crate::{
    cell::CellImmutable,
    cell_map::CellMap,
    traits::{CellValue, HasForeignKey, IdFor, JoinKeyFrom},
};

pub trait LeftSemiJoinOnExt<K, V>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    /// Keeps left rows that have at least one matching right row, using FK/ID conventions.
    ///
    /// Matching uses `left_value.fk()` against the right side's ID mapping.
    fn left_semi_join_on<RK, RV, RM>(
        &self,
        right: &CellMap<RK, RV, RM>,
    ) -> CellMap<K, V, CellImmutable>
    where
        RK: Hash + Eq + CellValue,
        RV: CellValue,
        V: HasForeignKey<RV>,
        <V as HasForeignKey<RV>>::ForeignKey: IdFor<RV>,
        <<V as HasForeignKey<RV>>::ForeignKey as IdFor<RV>>::MapKey: Hash + Eq + CellValue,
        RK: IdFor<RV>,
        <<V as HasForeignKey<RV>>::ForeignKey as IdFor<RV>>::MapKey:
            JoinKeyFrom<<RK as IdFor<RV>>::MapKey>;
}

impl<K, V, M> LeftSemiJoinOnExt<K, V> for CellMap<K, V, M>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    fn left_semi_join_on<RK, RV, RM>(
        &self,
        right: &CellMap<RK, RV, RM>,
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
    {
        self.left_semi_join_on_by(
            right,
            |_, value| value.fk().map_key(),
            |key, _| {
                <<V as HasForeignKey<RV>>::ForeignKey as IdFor<RV>>::MapKey::join_key_from(
                    &key.map_key(),
                )
            },
        )
    }
}
