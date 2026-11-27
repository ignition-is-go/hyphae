use rustc_hash::FxHashMap;
use uuid::Uuid;

/// Longest bucket searched linearly for a cell. Wider buckets index their
/// cells by id.
const SCAN_LIMIT: usize = 8;

/// The queued cells at one height, in the order they were first queued there.
///
/// A removed cell leaves an empty slot behind, so positions held by `index`
/// stay valid; the bucket is consumed whole when its height drains.
pub(super) struct Bucket<V> {
    height: u64,
    entries: Vec<(Uuid, Option<V>)>,
    live: usize,
    /// Position in `entries` of each live cell. Empty until `entries`
    /// outgrows [`SCAN_LIMIT`].
    index: FxHashMap<Uuid, usize>,
}

impl<V> Bucket<V> {
    const fn empty() -> Self {
        Self {
            height: 0,
            entries: Vec::new(),
            live: 0,
            index: FxHashMap::with_hasher(rustc_hash::FxBuildHasher),
        }
    }

    fn position(&self, id: Uuid) -> Option<usize> {
        if self.entries.len() > SCAN_LIMIT {
            return self.index.get(&id).copied();
        }
        self.entries
            .iter()
            .position(|(candidate, value)| *candidate == id && value.is_some())
    }

    fn insert(&mut self, id: Uuid, value: V) -> Option<V> {
        if let Some(position) = self.position(id) {
            return self
                .entries
                .get_mut(position)
                .and_then(|(_, slot)| slot.replace(value));
        }
        let position = self.entries.len();
        if position == SCAN_LIMIT {
            for (index, (live, slot)) in self.entries.iter().enumerate() {
                if slot.is_some() {
                    self.index.insert(*live, index);
                }
            }
        }
        if position >= SCAN_LIMIT {
            self.index.insert(id, position);
        }
        self.entries.push((id, Some(value)));
        self.live = self.live.saturating_add(1);
        None
    }

    fn get_mut(&mut self, id: Uuid) -> Option<&mut V> {
        let position = self.position(id)?;
        self.entries.get_mut(position)?.1.as_mut()
    }

    fn remove(&mut self, id: Uuid) -> Option<V> {
        let position = self.position(id)?;
        if self.entries.len() > SCAN_LIMIT {
            self.index.remove(&id);
        }
        let value = self.entries.get_mut(position)?.1.take()?;
        self.live = self.live.saturating_sub(1);
        Some(value)
    }

    pub(super) const fn height(&self) -> u64 {
        self.height
    }

    /// Every live cell, in the order it was first queued at this height.
    pub(super) fn drain(&mut self) -> impl Iterator<Item = (Uuid, V)> + '_ {
        self.live = 0;
        self.index.clear();
        self.entries
            .drain(..)
            .filter_map(|(id, value)| Some((id, value?)))
    }
}

/// The queued cells of a tick, grouped by height. Each height's cells drain
/// together, in the order they were first queued at that height.
///
/// Queuing a cell pushes it onto its height's bucket instead of inserting it
/// in one tree ordered by `(height, id)`: cells at one height are
/// independent, so no order among them is owed, and a wide wave costs one
/// push per cell rather than a tree insert and a tree removal.
pub(super) struct Frontier<V> {
    /// Buckets with live cells, highest height first, so the next wave is
    /// the last bucket.
    heights: Vec<Bucket<V>>,
    /// Drained buckets kept for reuse, so a steady tick allocates nothing.
    spare: Vec<Bucket<V>>,
}

impl<V> Frontier<V> {
    pub(super) const fn new() -> Self {
        Self {
            heights: Vec::new(),
            spare: Vec::new(),
        }
    }

    fn find(&self, height: u64) -> Result<usize, usize> {
        self.heights
            .binary_search_by(|bucket| height.cmp(&bucket.height))
    }

    pub(super) fn insert(&mut self, (height, id): (u64, Uuid), value: V) -> Option<V> {
        let position = match self.find(height) {
            Ok(position) => position,
            Err(position) => {
                let mut bucket = self.spare.pop().unwrap_or_else(Bucket::empty);
                bucket.height = height;
                self.heights.insert(position, bucket);
                position
            }
        };
        self.heights.get_mut(position)?.insert(id, value)
    }

    pub(super) fn get_mut(&mut self, &(height, id): &(u64, Uuid)) -> Option<&mut V> {
        let position = self.find(height).ok()?;
        self.heights.get_mut(position)?.get_mut(id)
    }

    pub(super) fn remove(&mut self, &(height, id): &(u64, Uuid)) -> Option<V> {
        let position = self.find(height).ok()?;
        let bucket = self.heights.get_mut(position)?;
        let value = bucket.remove(id);
        if bucket.live == 0 {
            let mut bucket = self.heights.remove(position);
            bucket.entries.clear();
            bucket.index.clear();
            self.spare.push(bucket);
        }
        value
    }

    /// Take the lowest height's bucket. Give it back with
    /// [`recycle`](Self::recycle) once drained.
    pub(super) fn pop_min(&mut self) -> Option<Bucket<V>> {
        self.heights.pop()
    }

    pub(super) fn recycle(&mut self, mut bucket: Bucket<V>) {
        bucket.entries.clear();
        bucket.index.clear();
        bucket.live = 0;
        self.spare.push(bucket);
    }

    pub(super) const fn is_empty(&self) -> bool {
        self.heights.is_empty()
    }

    #[cfg(test)]
    pub(super) fn get(&self, &(height, id): &(u64, Uuid)) -> Option<&V> {
        let bucket = self.heights.get(self.find(height).ok()?)?;
        bucket.entries.get(bucket.position(id)?)?.1.as_ref()
    }

    #[cfg(test)]
    pub(super) fn for_each(&self, mut visit: impl FnMut(&(u64, Uuid), &V)) {
        for bucket in self.heights.iter().rev() {
            for (id, value) in &bucket.entries {
                if let Some(value) = value {
                    visit(&(bucket.height, *id), value);
                }
            }
        }
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.heights.iter().map(|bucket| bucket.live).sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drain_min(frontier: &mut Frontier<u32>) -> Option<(u64, Vec<u32>)> {
        let mut bucket = frontier.pop_min()?;
        let height = bucket.height();
        let values = bucket.drain().map(|(_, value)| value).collect();
        frontier.recycle(bucket);
        Some((height, values))
    }

    #[test]
    fn drains_by_height_in_arrival_order_and_reuses_buckets() {
        let mut frontier = Frontier::new();
        for value in (0u32..64).rev() {
            let id = Uuid::from_u128(u128::from(value));
            assert!(frontier.insert((u64::from(value % 3), id), value).is_none());
        }
        assert_eq!(frontier.len(), 64);
        for height in 0..3 {
            let expected = (0u32..64)
                .rev()
                .filter(|value| u64::from(value % 3) == height)
                .collect::<Vec<_>>();
            assert_eq!(drain_min(&mut frontier), Some((height, expected)));
        }
        assert!(frontier.is_empty());
        assert_eq!(frontier.spare.len(), 3);
        assert!(frontier.insert((7, Uuid::from_u128(1)), 1).is_none());
        assert_eq!(frontier.spare.len(), 2);
        assert_eq!(drain_min(&mut frontier), Some((7, vec![1])));
    }

    #[test]
    fn replace_and_remove_find_cells_on_both_sides_of_the_scan_limit() {
        let mut frontier = Frontier::new();
        let ids = (0u128..20).map(Uuid::from_u128).collect::<Vec<_>>();
        for (value, &id) in (0u32..).zip(&ids) {
            assert!(frontier.insert((1, id), value).is_none());
        }
        for (value, &id) in (100u32..).zip(&ids) {
            assert!(frontier.insert((1, id), value).is_some());
        }
        for &id in ids.iter().step_by(2) {
            assert!(frontier.remove(&(1, id)).is_some());
            assert!(frontier.remove(&(1, id)).is_none());
        }
        // A removed cell queued again takes a new place at the end.
        if let Some(&first) = ids.first() {
            assert!(frontier.insert((1, first), 7).is_none());
        }
        let expected = (101u32..120).step_by(2).chain([7]).collect::<Vec<_>>();
        assert_eq!(drain_min(&mut frontier), Some((1, expected)));
        assert!(frontier.is_empty());
    }

    #[test]
    fn removing_the_last_cell_of_a_height_releases_it() {
        let mut frontier = Frontier::new();
        let id = Uuid::from_u128(1);
        assert!(frontier.insert((4, id), 1).is_none());
        assert!(frontier.insert((2, id), 2).is_none());
        assert_eq!(frontier.remove(&(2, id)), Some(2));
        assert_eq!(drain_min(&mut frontier), Some((4, vec![1])));
        assert!(frontier.is_empty());
    }
}
