use std::hash::Hash;

use rustc_hash::FxHashSet;

enum Storage<T> {
    Empty,
    One(T),
    Many { seen: FxHashSet<T>, values: Vec<T> },
}

/// Reusable insertion-ordered set. Query membership buckets and changed-key
/// lists commonly contain one key; those need neither a table nor a vector.
/// Once promoted, a set retains its large storage across ordinary clears.
pub(super) struct OrderedSet<T> {
    storage: Storage<T>,
}

impl<T> Default for OrderedSet<T> {
    fn default() -> Self {
        Self {
            storage: Storage::Empty,
        }
    }
}

impl<T> OrderedSet<T>
where
    T: Hash + Eq + Clone,
{
    #[cfg(test)]
    fn heap_slots(&self) -> usize {
        match &self.storage {
            Storage::Empty | Storage::One(_) => 0,
            Storage::Many { seen, values } => seen.capacity().saturating_add(values.capacity()),
        }
    }

    pub(super) fn insert(&mut self, value: T) -> bool {
        match &mut self.storage {
            Storage::Empty => {
                self.storage = Storage::One(value);
                return true;
            }
            Storage::One(previous) => {
                if previous == &value {
                    return false;
                }
            }
            Storage::Many { seen, values } => {
                if seen.insert(value.clone()) {
                    values.push(value);
                    return true;
                }
                return false;
            }
        }
        let previous = std::mem::replace(&mut self.storage, Storage::Empty);
        let Storage::One(first) = previous else {
            self.storage = previous;
            return false;
        };
        let values = vec![first, value];
        let seen = values.iter().cloned().collect();
        self.storage = Storage::Many { seen, values };
        true
    }

    pub(super) fn extend(&mut self, values: impl IntoIterator<Item = T>) {
        for value in values {
            self.insert(value);
        }
    }

    pub(super) fn drain(&mut self) -> impl Iterator<Item = T> + use<T> {
        let previous = std::mem::replace(&mut self.storage, Storage::Empty);
        let (single, values) = match previous {
            Storage::Empty => (None, Vec::new()),
            Storage::One(value) => (Some(value), Vec::new()),
            Storage::Many { mut seen, values } => {
                seen.clear();
                self.storage = Storage::Many {
                    seen,
                    values: Vec::new(),
                };
                (None, values)
            }
        };
        single.into_iter().chain(values)
    }

    /// Move ordered values into a reusable sibling buffer. Large sets swap
    /// vector allocations; the inline case appends without allocating a set.
    pub(super) fn move_into(&mut self, buffer: &mut Vec<T>) {
        buffer.clear();
        let previous = std::mem::replace(&mut self.storage, Storage::Empty);
        match previous {
            Storage::Empty => {}
            Storage::One(value) => buffer.push(value),
            Storage::Many {
                mut seen,
                mut values,
            } => {
                seen.clear();
                std::mem::swap(&mut values, buffer);
                self.storage = Storage::Many { seen, values };
            }
        }
    }

    pub(super) fn clear(&mut self) {
        match &mut self.storage {
            Storage::Empty => {}
            Storage::One(_) => self.storage = Storage::Empty,
            Storage::Many { seen, values } => {
                seen.clear();
                values.clear();
            }
        }
    }

    pub(super) fn contains(&self, value: &T) -> bool {
        match &self.storage {
            Storage::Empty => false,
            Storage::One(previous) => previous == value,
            Storage::Many { seen, .. } => seen.contains(value),
        }
    }

    pub(super) fn iter(&self) -> std::slice::Iter<'_, T> {
        match &self.storage {
            Storage::Empty => [].iter(),
            Storage::One(value) => std::slice::from_ref(value).iter(),
            Storage::Many { values, .. } => values.iter(),
        }
    }

    pub(super) fn remove(&mut self, value: &T) -> bool {
        match &mut self.storage {
            Storage::Empty => false,
            Storage::One(previous) => {
                if previous == value {
                    self.storage = Storage::Empty;
                    true
                } else {
                    false
                }
            }
            Storage::Many { seen, values } => {
                if !seen.remove(value) {
                    return false;
                }
                values.retain(|candidate| candidate != value);
                true
            }
        }
    }

    pub(super) const fn is_empty(&self) -> bool {
        match &self.storage {
            Storage::Empty => true,
            Storage::One(_) => false,
            Storage::Many { values, .. } => values.is_empty(),
        }
    }

    pub(super) fn retain(&mut self, mut keep: impl FnMut(&T) -> bool) {
        match &mut self.storage {
            Storage::Empty => {}
            Storage::One(value) => {
                if !keep(value) {
                    self.storage = Storage::Empty;
                }
            }
            Storage::Many { seen, values } => {
                values.retain(|value| {
                    let retained = keep(value);
                    if !retained {
                        seen.remove(value);
                    }
                    retained
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::OrderedSet;

    #[test]
    fn single_member_set_needs_no_heap_storage() {
        let mut set = OrderedSet::default();
        assert!(set.insert(7_usize));
        assert!(!set.insert(7));
        assert_eq!(set.iter().copied().collect::<Vec<_>>(), vec![7]);
        eprintln!("single-member set heap slots: {}", set.heap_slots());
        assert_eq!(set.heap_slots(), 0);
        assert_eq!(set.drain().collect::<Vec<_>>(), vec![7]);
        assert!(set.is_empty());
        assert!(set.insert(8));
        assert_eq!(set.heap_slots(), 0);
    }

    #[test]
    fn mutations_preserve_insertion_order_across_storage_transitions() {
        let mut set = OrderedSet::default();
        let mut expected = Vec::new();
        let mut buffer = Vec::new();
        let mut random = 7_usize;
        for step in 0..2_000_usize {
            random = random.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let value = (random >> 4).checked_rem(19).unwrap_or(0);
            match step.checked_rem(13).unwrap_or(0) {
                0 => {
                    set.clear();
                    expected.clear();
                }
                1 => {
                    set.move_into(&mut buffer);
                    assert_eq!(buffer, expected);
                    expected.clear();
                }
                2 => {
                    assert_eq!(set.drain().collect::<Vec<_>>(), expected);
                    expected.clear();
                }
                3 => {
                    let existed = expected.contains(&value);
                    assert_eq!(set.remove(&value), existed);
                    expected.retain(|v| *v != value);
                }
                4 => {
                    set.retain(|v| v.checked_rem(3) != Some(0));
                    expected.retain(|v| v.checked_rem(3) != Some(0));
                }
                _ => {
                    let is_new = !expected.contains(&value);
                    assert_eq!(set.insert(value), is_new);
                    if is_new {
                        expected.push(value);
                    }
                }
            }
            assert_eq!(set.iter().copied().collect::<Vec<_>>(), expected);
            assert_eq!(set.is_empty(), expected.is_empty());
            for candidate in 0..19 {
                assert_eq!(set.contains(&candidate), expected.contains(&candidate));
            }
        }
    }
}
