//! Right rows matched by one join key, as the contiguous slice handed to
//! projection closures.
//!
//! Closures see the rows in insertion order (an update keeps its row's
//! place), so the rows stay in one order-preserving `Vec`. A key can hold
//! thousands of rows; once a group is large, every row carries an increasing
//! stamp, and a key-to-stamp map plus a binary search over the stamps finds a
//! row without scanning or comparing the rows. Rows live in a ring buffer,
//! so removal shifts only the shorter side (removing the oldest or newest row
//! is constant time), and the ring is made contiguous again before any reader
//! can see it.
use std::{collections::VecDeque, hash::Hash};

use rustc_hash::FxHashMap;

/// Groups up to this size are scanned linearly; that beats hashing for the
/// common one-to-few group.
const LINEAR_MAX: usize = 16;

pub(in crate::traits::collections::internal) struct GroupedRows<RK, RV> {
    /// Always contiguous between mutations, so the first slice is every row.
    rows: VecDeque<(RK, RV)>,
    /// Stamp of each row in `rows`, strictly increasing; empty while linear.
    stamps: VecDeque<u64>,
    /// Stamp of every row key; empty while linear.
    stamp_of: FxHashMap<RK, u64>,
    next_stamp: u64,
}

impl<RK, RV> Default for GroupedRows<RK, RV> {
    fn default() -> Self {
        Self {
            rows: VecDeque::new(),
            stamps: VecDeque::new(),
            stamp_of: FxHashMap::default(),
            next_stamp: 0,
        }
    }
}

impl<RK: Clone, RV: Clone> Clone for GroupedRows<RK, RV> {
    fn clone(&self) -> Self {
        Self {
            rows: self.rows.clone(),
            stamps: self.stamps.clone(),
            stamp_of: self.stamp_of.clone(),
            next_stamp: self.next_stamp,
        }
    }
}

impl<RK, RV> GroupedRows<RK, RV> {
    pub(in crate::traits::collections::internal) fn as_slice(&self) -> &[(RK, RV)] {
        let (rows, wrapped) = self.rows.as_slices();
        debug_assert!(wrapped.is_empty(), "grouped rows left non-contiguous");
        rows
    }

    pub(in crate::traits::collections::internal) fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}

impl<RK: Hash + Eq + Clone, RV> GroupedRows<RK, RV> {
    fn is_linear(&self) -> bool {
        self.stamp_of.is_empty()
    }

    fn position(&self, key: &RK) -> Option<usize> {
        if self.is_linear() {
            return self.rows.iter().position(|(candidate, _)| candidate == key);
        }
        let stamp = self.stamp_of.get(key)?;
        self.stamps.binary_search(stamp).ok()
    }

    /// Replace the value of `key` in place, or append it.
    pub(in crate::traits::collections::internal) fn upsert(&mut self, key: RK, value: RV) {
        if let Some(index) = self.position(&key) {
            if let Some((_, slot)) = self.rows.get_mut(index) {
                *slot = value;
            }
            return;
        }
        if !self.is_linear() {
            let stamp = self.next_stamp;
            self.next_stamp = stamp.wrapping_add(1);
            self.stamp_of.insert(key.clone(), stamp);
            self.stamps.push_back(stamp);
        }
        self.rows.push_back((key, value));
        self.rows.make_contiguous();
        if self.is_linear() && self.rows.len() > LINEAR_MAX {
            self.promote();
        }
    }

    /// Remove `key`, keeping the order of the rest.
    pub(in crate::traits::collections::internal) fn remove(&mut self, key: &RK) {
        let Some(index) = self.position(key) else {
            return;
        };
        self.rows.remove(index);
        self.rows.make_contiguous();
        if !self.is_linear() {
            self.stamps.remove(index);
            self.stamp_of.remove(key);
            if self.rows.len() <= LINEAR_MAX / 2 {
                self.stamps = VecDeque::new();
                self.stamp_of = FxHashMap::default();
            }
        }
    }

    fn promote(&mut self) {
        self.stamp_of.reserve(self.rows.len());
        self.stamps.reserve(self.rows.len());
        for (key, _) in &self.rows {
            let stamp = self.next_stamp;
            self.next_stamp = stamp.wrapping_add(1);
            self.stamp_of.insert(key.clone(), stamp);
            self.stamps.push_back(stamp);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::GroupedRows;

    fn keys(group: &GroupedRows<u32, u32>) -> Vec<(u32, u32)> {
        group.as_slice().to_vec()
    }

    /// Reference model: the `Vec` scan the group replaces.
    fn model_upsert(rows: &mut Vec<(u32, u32)>, key: u32, value: u32) {
        if let Some((_, slot)) = rows.iter_mut().find(|(k, _)| *k == key) {
            *slot = value;
        } else {
            rows.push((key, value));
        }
    }

    #[test]
    fn matches_vec_model_across_promotion_and_demotion() {
        let mut group = GroupedRows::default();
        let mut model = Vec::new();
        let mut seed = 7_u32;
        for step in 0..20_000_u32 {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            let key = (seed >> 8) % if step % 4000 < 2000 { 64 } else { 12 };
            if seed.is_multiple_of(3) {
                group.remove(&key);
                model.retain(|(k, _)| *k != key);
            } else {
                group.upsert(key, step);
                model_upsert(&mut model, key, step);
            }
            assert_eq!(keys(&group), model, "step {step}");
        }
    }

    #[test]
    fn update_keeps_position_and_reinsert_appends() {
        let mut group = GroupedRows::default();
        for key in 0..40 {
            group.upsert(key, 0);
        }
        group.upsert(3, 9);
        assert_eq!(group.as_slice().get(3), Some(&(3, 9)));
        group.remove(&3);
        group.upsert(3, 1);
        assert_eq!(group.as_slice().last(), Some(&(3, 1)));
        assert_eq!(group.as_slice().len(), 40);
    }
}
