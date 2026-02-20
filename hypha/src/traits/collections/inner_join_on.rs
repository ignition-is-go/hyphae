use std::hash::Hash;

use super::InnerJoinOnByExt;
use crate::{
    cell::CellImmutable,
    cell_map::CellMap,
    traits::{CellValue, HasForeignKey, IdFor, JoinKeyFrom},
};

pub trait InnerJoinOnExt<K, V>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    /// Performs an inner join using FK/ID conventions.
    ///
    /// Rows are paired when `left_value.fk()` matches the right map key's ID mapping.
    /// `compute(&left_key, &left_value, &right_key, &right_value)` creates the output row.
    fn inner_join_on<RK, RV, RM, OK, OV, FC>(
        &self,
        right: &CellMap<RK, RV, RM>,
        compute: FC,
    ) -> CellMap<OK, OV, CellImmutable>
    where
        RK: Hash + Eq + CellValue,
        RV: CellValue,
        V: HasForeignKey<RV>,
        <V as HasForeignKey<RV>>::ForeignKey: IdFor<RV>,
        <<V as HasForeignKey<RV>>::ForeignKey as IdFor<RV>>::MapKey: Hash + Eq + CellValue,
        RK: IdFor<RV>,
        <<V as HasForeignKey<RV>>::ForeignKey as IdFor<RV>>::MapKey:
            JoinKeyFrom<<RK as IdFor<RV>>::MapKey>,
        OK: Hash + Eq + CellValue,
        OV: CellValue,
        FC: Fn(&K, &V, &RK, &RV) -> (OK, OV) + Send + Sync + 'static;
}

impl<K, V, M> InnerJoinOnExt<K, V> for CellMap<K, V, M>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    #[track_caller]
    fn inner_join_on<RK, RV, RM, OK, OV, FC>(
        &self,
        right: &CellMap<RK, RV, RM>,
        compute: FC,
    ) -> CellMap<OK, OV, CellImmutable>
    where
        RK: Hash + Eq + CellValue,
        RV: CellValue,
        V: HasForeignKey<RV>,
        <V as HasForeignKey<RV>>::ForeignKey: IdFor<RV>,
        <<V as HasForeignKey<RV>>::ForeignKey as IdFor<RV>>::MapKey: Hash + Eq + CellValue,
        RK: IdFor<RV>,
        <<V as HasForeignKey<RV>>::ForeignKey as IdFor<RV>>::MapKey:
            JoinKeyFrom<<RK as IdFor<RV>>::MapKey>,
        OK: Hash + Eq + CellValue,
        OV: CellValue,
        FC: Fn(&K, &V, &RK, &RV) -> (OK, OV) + Send + Sync + 'static,
    {
        self.inner_join_on_by(
            right,
            |_, value| value.fk().map_key(),
            |key, _| {
                <<V as HasForeignKey<RV>>::ForeignKey as IdFor<RV>>::MapKey::join_key_from(
                    &key.map_key(),
                )
            },
            compute,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::{Gettable, HasForeignKey, IdFor, IdType};

    impl IdFor<(String, i32)> for String {
        type MapKey = String;

        fn map_key(&self) -> Self::MapKey {
            self.clone()
        }
    }
    impl IdType for String {
        type Parent = (String, i32);
    }
    impl HasForeignKey<(String, i32)> for (String, i32) {
        type ForeignKey = String;

        fn fk(&self) -> Self::ForeignKey {
            self.0.clone()
        }
    }

    #[test]
    fn inner_join_on_reacts_to_both_sides_without_side_emission_growth() {
        let left = CellMap::<String, (String, i32)>::new();
        let right = CellMap::<String, (String, i32)>::new();
        let joined =
            left.inner_join_on(&right, |lk, lv, rk, rv| (format!("{lk}:{rk}"), lv.1 + rv.1));

        left.insert("l1".to_string(), ("g1".to_string(), 10));
        assert_eq!(joined.entries().get().len(), 0);

        right.insert("g1".to_string(), ("g1".to_string(), 5));
        assert_eq!(joined.get_value(&"l1:g1".to_string()), Some(15));
        right.insert("g1".to_string(), ("g1".to_string(), 7));
        assert_eq!(joined.get_value(&"l1:g1".to_string()), Some(17));
    }
}
