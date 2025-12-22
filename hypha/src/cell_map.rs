//! Reactive HashMap with per-key observability.
//!
//! `CellMap` wraps a concurrent HashMap where each entry can be individually observed.
//! Changes to keys trigger reactive updates to observers.

use std::hash::Hash;
use std::marker::PhantomData;
use std::sync::Arc;

use dashmap::DashMap;

use crate::cell::{Cell, CellImmutable, CellMutable, WeakCell};
use crate::traits::Mutable;

/// Diff notification for map changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MapDiff<K, V> {
    /// A new key was inserted.
    Insert { key: K, value: V },
    /// A key was removed.
    Remove { key: K, old_value: V },
    /// An existing key's value was updated.
    Update { key: K, old_value: V, new_value: V },
}

struct CellMapInner<K, V>
where
    K: Hash + Eq + Clone + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
{
    /// The actual data storage.
    data: DashMap<K, V>,
    /// Cached per-key observation cells.
    key_cells: DashMap<K, WeakCell<Option<V>, CellMutable>>,
    /// Cell for all entries.
    entries_cell: Cell<Vec<(K, V)>, CellMutable>,
    /// Cell for diff notifications.
    diffs_cell: Cell<Option<MapDiff<K, V>>, CellMutable>,
    /// Cell for length.
    len_cell: Cell<usize, CellMutable>,
}

/// A reactive HashMap with per-key observability.
///
/// # Example
///
/// ```
/// use hypha::{CellMap, Gettable, Watchable, Signal};
///
/// let map = CellMap::<String, i32>::new();
///
/// // Observe a specific key
/// let value_cell = map.get(&"counter".to_string());
/// assert_eq!(value_cell.get(), None);
///
/// // Insert triggers update
/// map.insert("counter".to_string(), 42);
/// assert_eq!(value_cell.get(), Some(42));
///
/// // Observe all entries
/// let entries = map.entries();
/// assert_eq!(entries.get().len(), 1);
/// ```
pub struct CellMap<K, V, M = CellMutable>
where
    K: Hash + Eq + Clone + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
{
    inner: Arc<CellMapInner<K, V>>,
    _marker: PhantomData<M>,
}

impl<K, V> CellMap<K, V, CellMutable>
where
    K: Hash + Eq + Clone + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
{
    /// Create a new empty CellMap.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(CellMapInner {
                data: DashMap::new(),
                key_cells: DashMap::new(),
                entries_cell: Cell::new(Vec::new()),
                diffs_cell: Cell::new(None),
                len_cell: Cell::new(0),
            }),
            _marker: PhantomData,
        }
    }

    /// Insert a key-value pair, returning the old value if present.
    pub fn insert(&self, key: K, value: V) -> Option<V> {
        let old = self.inner.data.insert(key.clone(), value.clone());

        // Emit diff
        let diff = match &old {
            Some(old_value) => MapDiff::Update {
                key: key.clone(),
                old_value: old_value.clone(),
                new_value: value.clone(),
            },
            None => MapDiff::Insert {
                key: key.clone(),
                value: value.clone(),
            },
        };
        self.inner.diffs_cell.set(Some(diff));

        // Update entries cell
        let entries: Vec<(K, V)> = self
            .inner
            .data
            .iter()
            .map(|r| (r.key().clone(), r.value().clone()))
            .collect();
        self.inner.entries_cell.set(entries);

        // Update len
        self.inner.len_cell.set(self.inner.data.len());

        // Notify per-key observers
        if let Some(weak) = self.inner.key_cells.get(&key) {
            if let Some(cell) = weak.upgrade() {
                cell.set(Some(value));
            }
        }

        old
    }

    /// Remove a key, returning the old value if present.
    pub fn remove(&self, key: &K) -> Option<V> {
        let removed = self.inner.data.remove(key);

        if let Some((k, old_value)) = removed {
            // Emit diff
            self.inner.diffs_cell.set(Some(MapDiff::Remove {
                key: k.clone(),
                old_value: old_value.clone(),
            }));

            // Update entries cell
            let entries: Vec<(K, V)> = self
                .inner
                .data
                .iter()
                .map(|r| (r.key().clone(), r.value().clone()))
                .collect();
            self.inner.entries_cell.set(entries);

            // Update len
            self.inner.len_cell.set(self.inner.data.len());

            // Notify per-key observers
            if let Some(weak) = self.inner.key_cells.get(&k) {
                if let Some(cell) = weak.upgrade() {
                    cell.set(None);
                }
            }

            Some(old_value)
        } else {
            None
        }
    }

    /// Lock the map to prevent further mutations.
    pub fn lock(self) -> CellMap<K, V, CellImmutable> {
        CellMap {
            inner: self.inner,
            _marker: PhantomData,
        }
    }
}

impl<K, V> Default for CellMap<K, V, CellMutable>
where
    K: Hash + Eq + Clone + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<K, V, M> CellMap<K, V, M>
where
    K: Hash + Eq + Clone + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
{
    /// Get an observable Cell for a specific key.
    ///
    /// Returns a `Cell<Option<V>>` that updates whenever the key's value changes.
    /// Multiple calls with the same key return the same underlying Cell.
    pub fn get(&self, key: &K) -> Cell<Option<V>, CellImmutable> {
        // Check cache first
        if let Some(weak) = self.inner.key_cells.get(key) {
            if let Some(cell) = weak.upgrade() {
                return cell.lock();
            }
        }

        // Create new cell with current value
        let current = self.inner.data.get(key).map(|r| r.value().clone());
        let cell = Cell::new(current);
        let weak = cell.downgrade();

        // Cache it
        self.inner.key_cells.insert(key.clone(), weak);

        cell.lock()
    }

    /// Get an observable Cell of all entries.
    pub fn entries(&self) -> Cell<Vec<(K, V)>, CellImmutable> {
        self.inner.entries_cell.clone().lock()
    }

    /// Get an observable Cell of all keys.
    pub fn keys(&self) -> Cell<Vec<K>, CellImmutable> {
        use crate::traits::MapExt;
        self.inner
            .entries_cell
            .map(|entries| entries.iter().map(|(k, _)| k.clone()).collect())
    }

    /// Get an observable Cell of the map length.
    pub fn len(&self) -> Cell<usize, CellImmutable> {
        self.inner.len_cell.clone().lock()
    }

    /// Check if map is empty (non-reactive).
    pub fn is_empty(&self) -> bool {
        self.inner.data.is_empty()
    }

    /// Get an observable Cell of diff notifications.
    ///
    /// Emits `Some(MapDiff)` on each insert/remove/update, starts with `None`.
    pub fn diffs(&self) -> Cell<Option<MapDiff<K, V>>, CellImmutable> {
        self.inner.diffs_cell.clone().lock()
    }

    /// Check if key exists (non-reactive).
    pub fn contains_key(&self, key: &K) -> bool {
        self.inner.data.contains_key(key)
    }

    /// Get current value for key (non-reactive).
    pub fn get_value(&self, key: &K) -> Option<V> {
        self.inner.data.get(key).map(|r| r.value().clone())
    }
}

impl<K, V, M> Clone for CellMap<K, V, M>
where
    K: Hash + Eq + Clone + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
{
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            _marker: PhantomData,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::{Gettable, Watchable};
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn test_cellmap_basic() {
        let map = CellMap::<String, i32>::new();

        assert!(map.is_empty());
        assert_eq!(map.get_value(&"a".to_string()), None);

        map.insert("a".to_string(), 1);
        assert_eq!(map.get_value(&"a".to_string()), Some(1));
        assert!(!map.is_empty());

        map.insert("b".to_string(), 2);
        assert_eq!(map.get_value(&"b".to_string()), Some(2));

        let old = map.remove(&"a".to_string());
        assert_eq!(old, Some(1));
        assert_eq!(map.get_value(&"a".to_string()), None);
    }

    #[test]
    fn test_cellmap_per_key_observation() {
        let map = CellMap::<String, i32>::new();

        // Get cell before key exists
        let cell_a = map.get(&"a".to_string());
        assert_eq!(cell_a.get(), None);

        let count = Arc::new(AtomicUsize::new(0));
        let c = count.clone();
        let _guard = cell_a.subscribe(move |_| {
            c.fetch_add(1, Ordering::SeqCst);
        });

        assert_eq!(count.load(Ordering::SeqCst), 1); // Initial

        // Insert should trigger update
        map.insert("a".to_string(), 42);
        assert_eq!(cell_a.get(), Some(42));
        assert_eq!(count.load(Ordering::SeqCst), 2);

        // Update should trigger
        map.insert("a".to_string(), 100);
        assert_eq!(cell_a.get(), Some(100));
        assert_eq!(count.load(Ordering::SeqCst), 3);

        // Remove should trigger
        map.remove(&"a".to_string());
        assert_eq!(cell_a.get(), None);
        assert_eq!(count.load(Ordering::SeqCst), 4);
    }

    #[test]
    fn test_cellmap_entries_observation() {
        let map = CellMap::<String, i32>::new();
        let entries = map.entries();

        assert_eq!(entries.get(), vec![]);

        map.insert("a".to_string(), 1);
        assert_eq!(entries.get().len(), 1);

        map.insert("b".to_string(), 2);
        assert_eq!(entries.get().len(), 2);

        map.remove(&"a".to_string());
        assert_eq!(entries.get().len(), 1);
    }

    #[test]
    fn test_cellmap_diffs() {
        let map = CellMap::<String, i32>::new();
        let diffs = map.diffs();

        assert_eq!(diffs.get(), None);

        map.insert("a".to_string(), 1);
        assert_eq!(
            diffs.get(),
            Some(MapDiff::Insert {
                key: "a".to_string(),
                value: 1
            })
        );

        map.insert("a".to_string(), 2);
        assert_eq!(
            diffs.get(),
            Some(MapDiff::Update {
                key: "a".to_string(),
                old_value: 1,
                new_value: 2
            })
        );

        map.remove(&"a".to_string());
        assert_eq!(
            diffs.get(),
            Some(MapDiff::Remove {
                key: "a".to_string(),
                old_value: 2
            })
        );
    }

    #[test]
    fn test_cellmap_len() {
        let map = CellMap::<String, i32>::new();
        let len = map.len();

        assert_eq!(len.get(), 0);

        map.insert("a".to_string(), 1);
        assert_eq!(len.get(), 1);

        map.insert("b".to_string(), 2);
        assert_eq!(len.get(), 2);

        map.remove(&"a".to_string());
        assert_eq!(len.get(), 1);
    }

    #[test]
    fn test_cellmap_lock() {
        let map = CellMap::<String, i32>::new();
        map.insert("a".to_string(), 1);

        let locked = map.lock();

        // Can still observe
        assert_eq!(locked.get(&"a".to_string()).get(), Some(1));
        assert_eq!(locked.entries().get().len(), 1);

        // But can't mutate - these methods don't exist on CellImmutable
        // locked.insert(...) // compile error
    }

    #[test]
    fn test_cellmap_same_cell_returned() {
        let map = CellMap::<String, i32>::new();

        let cell1 = map.get(&"a".to_string());
        let cell2 = map.get(&"a".to_string());

        // Both should reflect same updates
        map.insert("a".to_string(), 42);

        assert_eq!(cell1.get(), Some(42));
        assert_eq!(cell2.get(), Some(42));
    }
}
