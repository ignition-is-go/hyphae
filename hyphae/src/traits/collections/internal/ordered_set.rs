use std::hash::Hash;

use rustc_hash::FxHashSet;

/// Reusable insertion-ordered set for deterministic incremental work lists.
pub(super) struct OrderedSet<T> {
    seen: FxHashSet<T>,
    values: Vec<T>,
}

impl<T> Default for OrderedSet<T> {
    fn default() -> Self {
        Self {
            seen: FxHashSet::default(),
            values: Vec::new(),
        }
    }
}

impl<T> OrderedSet<T>
where
    T: Hash + Eq + Clone,
{
    pub(super) fn insert(&mut self, value: T) -> bool {
        if self.seen.insert(value.clone()) {
            self.values.push(value);
            true
        } else {
            false
        }
    }

    pub(super) fn extend(&mut self, values: impl IntoIterator<Item = T>) {
        for value in values {
            self.insert(value);
        }
    }

    pub(super) fn drain(&mut self) -> std::vec::IntoIter<T> {
        self.seen.clear();
        std::mem::take(&mut self.values).into_iter()
    }

    pub(super) fn clear(&mut self) {
        self.seen.clear();
        self.values.clear();
    }

    pub(super) fn contains(&self, value: &T) -> bool {
        self.seen.contains(value)
    }

    pub(super) fn iter(&self) -> std::slice::Iter<'_, T> {
        self.values.iter()
    }

    pub(super) fn remove(&mut self, value: &T) -> bool {
        if !self.seen.remove(value) {
            return false;
        }
        self.values.retain(|candidate| candidate != value);
        true
    }

    pub(super) const fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    pub(super) fn retain(&mut self, mut keep: impl FnMut(&T) -> bool) {
        let seen = &mut self.seen;
        self.values.retain(|value| {
            let retained = keep(value);
            if !retained {
                seen.remove(value);
            }
            retained
        });
    }
}
