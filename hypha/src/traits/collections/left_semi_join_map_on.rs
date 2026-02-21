use std::hash::Hash;

use super::LeftSemiJoinMapOnByExt;
use crate::{
    cell::CellImmutable,
    cell_map::CellMap,
    traits::{CellValue, HasForeignKey, IdFor, JoinKeyFrom},
};

pub trait LeftSemiJoinMapOnExt<K, V>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    /// Keeps left rows with at least one matching right row by FK/ID conventions, then maps.
    fn left_semi_join_map_on<RK, RV, RM, U, FM>(
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
        FM: Fn(&K, &V, &RV) -> U + Send + Sync + 'static;
}

impl<K, V, M> LeftSemiJoinMapOnExt<K, V> for CellMap<K, V, M>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    fn left_semi_join_map_on<RK, RV, RM, U, FM>(
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
        FM: Fn(&K, &V, &RV) -> U + Send + Sync + 'static,
    {
        self.left_semi_join_map_on_by(
            right,
            |_, left_value| left_value.fk().map_key(),
            |right_key, _| {
                <<V as HasForeignKey<RV>>::ForeignKey as IdFor<RV>>::MapKey::join_key_from(
                    &right_key.map_key(),
                )
            },
            mapper,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::{HasForeignKey, IdFor, IdType};

    #[derive(Clone, Debug, PartialEq, Eq, Hash)]
    struct ParentId(String);

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct ParentRow;

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct ChildRow {
        parent_id: ParentId,
        value: i32,
    }

    impl IdFor<ParentRow> for ParentId {
        type MapKey = String;

        fn map_key(&self) -> Self::MapKey {
            self.0.clone()
        }
    }

    impl IdType for ParentId {
        type Parent = ParentRow;
    }

    impl HasForeignKey<ParentRow> for ChildRow {
        type ForeignKey = ParentId;

        fn fk(&self) -> Self::ForeignKey {
            self.parent_id.clone()
        }
    }

    #[test]
    fn left_semi_join_map_on_uses_fk_defaults() {
        let left = CellMap::<String, ChildRow>::new();
        let right = CellMap::<ParentId, ParentRow>::new();
        let out = left.left_semi_join_map_on(&right, |_, lv, _| lv.value);

        left.insert(
            "c1".to_string(),
            ChildRow {
                parent_id: ParentId("p1".to_string()),
                value: 9,
            },
        );
        assert_eq!(out.get_value(&"c1".to_string()), None);

        right.insert(ParentId("p1".to_string()), ParentRow);
        assert_eq!(out.get_value(&"c1".to_string()), Some(9));
    }
}
