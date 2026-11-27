//! Members of one join-index bucket: insertion-ordered, with constant-time
//! insert, remove and membership once a bucket is large.
//!
//! A join key can hold thousands of rows (every node of one engine), so a
//! plain `Vec` with `contains`/`retain` made building and updating such a
//! bucket quadratic. Readers iterate members in insertion order — that order
//! reaches matched-row slices — so removal leaves a tombstone instead of
//! swapping, and the slots are compacted once tombstones outnumber members.
use std::hash::Hash;

use rustc_hash::FxHashMap;

/// Buckets up to this size are scanned linearly and removed by shifting;
/// that beats hashing for the common one-to-few bucket.
const LINEAR_MAX: usize = 16;

pub(in crate::traits::collections::internal) struct IndexMembers<M> {
    slots: Vec<Option<M>>,
    /// Slot of every live member; empty while the bucket is linear.
    positions: FxHashMap<M, usize>,
    live: usize,
}

impl<M> Default for IndexMembers<M> {
    fn default() -> Self {
        Self {
            slots: Vec::new(),
            positions: FxHashMap::default(),
            live: 0,
        }
    }
}

impl<M: Clone> Clone for IndexMembers<M> {
    fn clone(&self) -> Self {
        Self {
            slots: self.slots.clone(),
            positions: self.positions.clone(),
            live: self.live,
        }
    }
}

impl<M: std::fmt::Debug> std::fmt::Debug for IndexMembers<M> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

impl<M> IndexMembers<M> {
    pub(in crate::traits::collections::internal) const fn len(&self) -> usize {
        self.live
    }

    pub(in crate::traits::collections::internal) const fn is_empty(&self) -> bool {
        self.live == 0
    }

    /// Members in insertion order.
    pub(in crate::traits::collections::internal) fn iter(
        &self,
    ) -> std::iter::Flatten<std::slice::Iter<'_, Option<M>>> {
        self.slots.iter().flatten()
    }
}

impl<M: Hash + Eq + Clone> IndexMembers<M> {
    fn is_linear(&self) -> bool {
        self.positions.is_empty()
    }

    /// Add `member` at the end unless present. Returns whether it was added.
    pub(in crate::traits::collections::internal) fn insert(&mut self, member: M) -> bool {
        if self.is_linear() {
            if self
                .slots
                .iter()
                .flatten()
                .any(|candidate| candidate == &member)
            {
                return false;
            }
            self.slots.push(Some(member));
            self.live = self.live.saturating_add(1);
            if self.slots.len() > LINEAR_MAX {
                self.promote();
            }
            return true;
        }
        if self.positions.contains_key(&member) {
            return false;
        }
        self.positions.insert(member.clone(), self.slots.len());
        self.slots.push(Some(member));
        self.live = self.live.saturating_add(1);
        true
    }

    /// Remove `member`, keeping the order of the rest. Returns whether it
    /// was present.
    pub(in crate::traits::collections::internal) fn remove(&mut self, member: &M) -> bool {
        if self.is_linear() {
            let Some(index) = self
                .slots
                .iter()
                .position(|candidate| candidate.as_ref() == Some(member))
            else {
                return false;
            };
            self.slots.remove(index);
            self.live = self.live.saturating_sub(1);
            return true;
        }
        let Some(index) = self.positions.remove(member) else {
            return false;
        };
        if let Some(slot) = self.slots.get_mut(index) {
            *slot = None;
        }
        self.live = self.live.saturating_sub(1);
        if self.slots.len() > self.live.saturating_mul(2) {
            self.compact();
        }
        true
    }

    fn promote(&mut self) {
        self.positions.reserve(self.slots.len());
        for (index, member) in self.slots.iter().enumerate() {
            if let Some(member) = member {
                self.positions.insert(member.clone(), index);
            }
        }
    }

    fn compact(&mut self) {
        self.slots.retain(Option::is_some);
        self.positions.clear();
        if self.slots.len() > LINEAR_MAX {
            self.promote();
        }
    }
}

impl<M: Hash + Eq + Clone> FromIterator<M> for IndexMembers<M> {
    fn from_iter<T: IntoIterator<Item = M>>(iter: T) -> Self {
        let mut members = Self::default();
        for member in iter {
            members.insert(member);
        }
        members
    }
}

impl<'a, M> IntoIterator for &'a IndexMembers<M> {
    type Item = &'a M;
    type IntoIter = std::iter::Flatten<std::slice::Iter<'a, Option<M>>>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

/// Add `member` to the bucket for `index_key`.
pub(in crate::traits::collections::internal) fn add_index_member<I, M>(
    index: &mut FxHashMap<I, IndexMembers<M>>,
    index_key: I,
    member: M,
) where
    I: Hash + Eq,
    M: Hash + Eq + Clone,
{
    index.entry(index_key).or_default().insert(member);
}

/// Remove `member` from the bucket for `index_key`, dropping an emptied bucket.
pub(in crate::traits::collections::internal) fn remove_index_member<I, M>(
    index: &mut FxHashMap<I, IndexMembers<M>>,
    index_key: &I,
    member: &M,
) where
    I: Hash + Eq,
    M: Hash + Eq + Clone,
{
    if let Some(members) = index.get_mut(index_key) {
        members.remove(member);
        if members.is_empty() {
            index.remove(index_key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::IndexMembers;

    fn members(set: &IndexMembers<u32>) -> Vec<u32> {
        set.iter().copied().collect()
    }

    #[test]
    fn keeps_insertion_order_across_promotion_removal_and_compaction() {
        let mut set = IndexMembers::default();
        let mut model = Vec::new();
        // Deterministic mix of inserts, duplicate inserts and removals.
        let mut seed = 7_u32;
        for _ in 0..20_000 {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            let value = (seed >> 8) % 200;
            if seed.is_multiple_of(3) {
                assert_eq!(set.remove(&value), model.contains(&value));
                model.retain(|candidate| candidate != &value);
            } else {
                assert_eq!(set.insert(value), !model.contains(&value));
                if !model.contains(&value) {
                    model.push(value);
                }
            }
            assert_eq!(set.len(), model.len());
        }
        assert_eq!(members(&set), model);
    }
}
