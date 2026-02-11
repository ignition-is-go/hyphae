//! Reactive HashMap with per-key observability.
//!
//! `CellMap` wraps a concurrent HashMap where each entry can be individually observed.
//! Changes to keys trigger reactive updates to observers.

use std::{hash::Hash, marker::PhantomData, sync::Arc};

use arc_swap::ArcSwap;
use dashmap::DashMap;
use uuid::Uuid;

use crate::{
    cell::{Cell, CellImmutable, CellMutable, WeakCell},
    signal::Signal,
    subscription::SubscriptionGuard,
    traits::{CellValue, Gettable, Mutable, Watchable},
};

/// Diff notification for map changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MapDiff<K, V> {
    /// Initial snapshot of all entries when subscribing.
    Initial { entries: Vec<(K, V)> },
    /// A new key was inserted.
    Insert { key: K, value: V },
    /// A key was removed.
    Remove { key: K, old_value: V },
    /// An existing key's value was updated.
    Update { key: K, old_value: V, new_value: V },
}

struct CellMapInner<K, V>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    /// The actual data storage.
    data: DashMap<K, V>,
    /// Cached per-key observation cells.
    key_cells: DashMap<K, WeakCell<Option<V>, CellMutable>>,
    /// Cell for diff notifications.
    diffs_cell: Cell<Option<MapDiff<K, V>>, CellMutable>,
    /// Cell for length.
    len_cell: Cell<usize, CellMutable>,
    /// Subscription guards owned by this map (dropped when map drops).
    owned: DashMap<Uuid, SubscriptionGuard>,
    /// Optional name for debugging.
    name: ArcSwap<Option<Arc<str>>>,
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
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    inner: Arc<CellMapInner<K, V>>,
    _marker: PhantomData<M>,
}

impl<K, V> CellMap<K, V, CellMutable>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    /// Create a new empty CellMap.
    #[track_caller]
    pub fn new() -> Self {
        let diffs_cell = Cell::new(None);
        let len_cell = Cell::new(0);

        // Mark len_cell as owned by diffs_cell so it doesn't appear as an orphan root
        #[cfg(all(feature = "inspector", not(target_arch = "wasm32")))]
        {
            use crate::traits::DepNode;
            crate::registry::registry().mark_owned(len_cell.id(), diffs_cell.id());
        }

        Self {
            inner: Arc::new(CellMapInner {
                data: DashMap::new(),
                key_cells: DashMap::new(),
                diffs_cell,
                len_cell,
                owned: DashMap::new(),
                name: ArcSwap::from_pointee(None),
            }),
            _marker: PhantomData,
        }
    }

    /// Own a subscription guard, keeping it alive as long as this CellMap exists.
    pub(crate) fn own(&self, guard: SubscriptionGuard) {
        #[cfg(all(feature = "inspector", not(target_arch = "wasm32")))]
        {
            use crate::traits::DepNode;
            // Use diffs_cell as the CellMap's representative identity in the inspector
            crate::registry::registry().mark_owned(guard.source().id(), self.inner.diffs_cell.id());
        }
        self.inner.owned.insert(Uuid::new_v4(), guard);
    }

    /// Insert a key-value pair, returning the old value if present.
    pub fn insert(&self, key: K, value: V) -> Option<V> {
        let old = self.inner.data.insert(key.clone(), value.clone());

        // Emit diff (O(1) - just notifies subscribers)
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

        // Update len (O(1))
        self.inner.len_cell.set(self.inner.data.len());

        // Notify per-key observers (O(1))
        if let Some(weak) = self.inner.key_cells.get(&key)
            && let Some(cell) = weak.upgrade()
        {
            cell.set(Some(value));
        }

        old
    }

    /// Remove a key, returning the old value if present.
    pub fn remove(&self, key: &K) -> Option<V> {
        let removed = self.inner.data.remove(key);

        if let Some((k, old_value)) = removed {
            // Emit diff (O(1) - just notifies subscribers)
            self.inner.diffs_cell.set(Some(MapDiff::Remove {
                key: k.clone(),
                old_value: old_value.clone(),
            }));

            // Update len (O(1))
            self.inner.len_cell.set(self.inner.data.len());

            // Notify per-key observers (O(1))
            if let Some(weak) = self.inner.key_cells.get(&k)
                && let Some(cell) = weak.upgrade()
            {
                cell.set(None);
            }

            Some(old_value)
        } else {
            None
        }
    }

    /// Give this CellMap a name for debugging. Names its internal cells accordingly.
    pub fn with_name(self, name: impl Into<Arc<str>>) -> Self {
        let name: Arc<str> = name.into();
        self.inner
            .diffs_cell
            .clone()
            .with_name(format!("{}::diffs", name));
        self.inner
            .len_cell
            .clone()
            .with_name(format!("{}::len", name));
        self.inner.name.store(Arc::new(Some(name)));
        self
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
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<K, V, M> CellMap<K, V, M>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    /// Get an observable Cell for a specific key.
    ///
    /// Returns a `Cell<Option<V>>` that updates whenever the key's value changes.
    /// Multiple calls with the same key return the same underlying Cell.
    #[track_caller]
    pub fn get(&self, key: &K) -> Cell<Option<V>, CellImmutable> {
        // Check cache first
        if let Some(weak) = self.inner.key_cells.get(key)
            && let Some(cell) = weak.upgrade()
        {
            return cell.lock();
        }

        // Create new cell with current value
        let current = self.inner.data.get(key).map(|r| r.value().clone());
        let cell = Cell::new(current);
        if let Some(map_name) = (**self.inner.name.load()).as_ref() {
            cell.clone().with_name(format!("{}[{:?}]", map_name, key));
        }

        // Mark per-key cell as owned by diffs_cell so it doesn't appear as an orphan root
        #[cfg(all(feature = "inspector", not(target_arch = "wasm32")))]
        {
            use crate::traits::DepNode;
            crate::registry::registry().mark_owned(cell.id(), self.inner.diffs_cell.id());
        }

        let weak = cell.downgrade();

        // Cache it
        self.inner.key_cells.insert(key.clone(), weak);

        cell.lock()
    }

    /// Get an observable Cell of all entries.
    ///
    /// Returns a derived cell that maintains entries incrementally via diffs.
    /// The initial value is computed from the current map state, then updates
    /// are applied incrementally as O(1) operations per diff.
    #[track_caller]
    pub fn entries(&self) -> Cell<Vec<(K, V)>, CellImmutable> {
        // Build initial entries from current data (O(N) once)
        let initial: Vec<(K, V)> = self
            .inner
            .data
            .iter()
            .map(|r| (r.key().clone(), r.value().clone()))
            .collect();

        let cell = Cell::new(initial);
        if let Some(map_name) = (**self.inner.name.load()).as_ref() {
            cell.clone().with_name(format!("{}::entries", map_name));
        }
        let weak_cell = cell.downgrade();

        // Keep CellMapInner alive as long as this subscription exists.
        // When select() uses a weak ref in its closure, the CellMapInner would otherwise
        // be dropped once the temporary CellMap from select() goes out of scope.
        // This keepalive ensures the filtered CellMap (and its source subscription) survive
        // as long as the entries Cell is alive.
        let map_keepalive = self.inner.clone();

        // Subscribe to diffs and apply incrementally (O(1) per update)
        let first = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let guard = self.inner.diffs_cell.subscribe(move |signal| {
            let _ = &map_keepalive; // prevent drop until closure is dropped
            // Skip the first signal (current value from Cell subscription)
            if first.swap(false, std::sync::atomic::Ordering::SeqCst) {
                return;
            }
            let Some(cell) = weak_cell.upgrade() else {
                return; // Entries cell was dropped
            };
            if let Signal::Value(arc_opt) = signal
                && let Some(diff) = arc_opt.as_ref()
            {
                // Apply diff incrementally to the entries
                let mut entries = cell.get();
                match diff {
                    MapDiff::Initial { entries: init } => {
                        entries = init.clone();
                    }
                    MapDiff::Insert { key, value } => {
                        entries.push((key.clone(), value.clone()));
                    }
                    MapDiff::Remove { key, .. } => {
                        entries.retain(|(k, _)| k != key);
                    }
                    MapDiff::Update { key, new_value, .. } => {
                        if let Some((_, v)) = entries.iter_mut().find(|(k, _)| k == key) {
                            *v = new_value.clone();
                        }
                    }
                }
                cell.set(entries);
            }
        });

        // Own the subscription guard — this also marks diffs_cell as owned by entries cell
        cell.own(guard);

        cell.lock()
    }

    /// Get an observable Cell of all keys.
    #[track_caller]
    pub fn keys(&self) -> Cell<Vec<K>, CellImmutable> {
        use crate::traits::MapExt;
        self.entries()
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

    /// Get a point-in-time snapshot of all entries (non-reactive).
    ///
    /// Unlike `entries()`, this does NOT create a Cell or subscribe to changes.
    /// Use this for one-shot reads where you don't need live updates.
    pub fn snapshot(&self) -> Vec<(K, V)> {
        self.inner
            .data
            .iter()
            .map(|r| (r.key().clone(), r.value().clone()))
            .collect()
    }

    /// Check if key exists (non-reactive).
    pub fn contains_key(&self, key: &K) -> bool {
        self.inner.data.contains_key(key)
    }

    /// Get current value for key (non-reactive).
    pub fn get_value(&self, key: &K) -> Option<V> {
        self.inner.data.get(key).map(|r| r.value().clone())
    }

    /// Subscribe to diffs with an initial snapshot.
    ///
    /// The callback is first called with `MapDiff::Initial` containing all current
    /// entries, then called with subsequent diffs as the map changes.
    ///
    /// Returns a guard that cancels the subscription when dropped.
    pub fn subscribe_diffs<F>(&self, callback: F) -> SubscriptionGuard
    where
        F: Fn(&MapDiff<K, V>) + Send + Sync + 'static,
    {
        // Emit initial snapshot
        let entries: Vec<(K, V)> = self
            .inner
            .data
            .iter()
            .map(|r| (r.key().clone(), r.value().clone()))
            .collect();
        callback(&MapDiff::Initial { entries });

        // Subscribe to subsequent diffs.
        // Capture a strong ref to CellMapInner so the map (and its owned subscription guards)
        // stays alive as long as this subscription exists. Without this, if the CellMap is
        // dropped (e.g., passed by value to subscribe_diffs then goes out of scope), the
        // CellMapInner and its owned guards would be dropped, breaking upstream subscriptions.
        let map_keepalive = self.inner.clone();
        let diffs = self.diffs();
        let first = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        diffs.subscribe(move |signal| {
            let _ = &map_keepalive;
            // Skip the first signal (the current value from Cell subscription)
            if first.swap(false, std::sync::atomic::Ordering::SeqCst) {
                return;
            }
            if let crate::Signal::Value(arc_opt) = signal
                && let Some(diff) = arc_opt.as_ref()
            {
                callback(diff);
            }
        })
    }
}

impl<K, V, M> Clone for CellMap<K, V, M>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            _marker: PhantomData,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// SelectExt - Extension trait for filtering CellMap values
// ─────────────────────────────────────────────────────────────────────────────

/// Extension trait for filtering CellMap values reactively.
pub trait SelectExt<K, V>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    /// Create a filtered view of this CellMap.
    ///
    /// Returns an immutable `CellMap` containing only entries where the
    /// predicate returns `true`. The filtered map automatically updates
    /// when the source map changes.
    ///
    /// # Example
    ///
    /// ```
    /// use hypha::{CellMap, Gettable, SelectExt};
    ///
    /// let map = CellMap::<String, i32>::new();
    /// map.insert("small".to_string(), 5);
    /// map.insert("large".to_string(), 100);
    ///
    /// let large_values = map.select(|v| *v > 50);
    /// assert_eq!(large_values.entries().get().len(), 1);
    ///
    /// // Adding a large value updates the filtered map
    /// map.insert("huge".to_string(), 1000);
    /// assert_eq!(large_values.entries().get().len(), 2);
    /// ```
    #[track_caller]
    fn select<F>(&self, predicate: F) -> CellMap<K, V, CellImmutable>
    where
        F: Fn(&V) -> bool + Send + Sync + 'static;
}

impl<K, V, M> SelectExt<K, V> for CellMap<K, V, M>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    #[track_caller]
    fn select<F>(&self, predicate: F) -> CellMap<K, V, CellImmutable>
    where
        F: Fn(&V) -> bool + Send + Sync + 'static,
    {
        let filtered = CellMap::<K, V, CellMutable>::new();
        if let Some(parent_name) = (**self.inner.name.load()).as_ref() {
            filtered
                .clone()
                .with_name(format!("{}::select", parent_name));
        }
        let predicate = Arc::new(predicate);

        // Use a weak reference to break the self-referential cycle:
        // Without this, CellMapInner.owned → guard → closure → filtered_clone → CellMapInner
        // would prevent the filtered CellMap from ever being dropped.
        // Downstream consumers (entries(), diffs()) keep CellMapInner alive via their callbacks.
        let filtered_weak = Arc::downgrade(&filtered.inner);
        let guard = self.subscribe_diffs(move |diff| {
            let Some(inner) = filtered_weak.upgrade() else {
                return; // Filtered map was dropped
            };
            let filtered = CellMap::<K, V, CellMutable> {
                inner,
                _marker: PhantomData,
            };
            match diff {
                MapDiff::Initial { entries } => {
                    for (key, value) in entries {
                        if predicate(value) {
                            filtered.insert(key.clone(), value.clone());
                        }
                    }
                }
                MapDiff::Insert { key, value } => {
                    if predicate(value) {
                        filtered.insert(key.clone(), value.clone());
                    }
                }
                MapDiff::Remove { key, .. } => {
                    filtered.remove(key);
                }
                MapDiff::Update { key, new_value, .. } => {
                    if predicate(new_value) {
                        filtered.insert(key.clone(), new_value.clone());
                    } else {
                        filtered.remove(key);
                    }
                }
            }
        });

        // Own the guard so it stays alive as long as the filtered map exists
        filtered.own(guard);

        // Return locked (immutable) view
        filtered.lock()
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_types)]
mod tests {
    use std::sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    };

    use super::*;
    use crate::traits::{Gettable, Watchable};

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
    fn test_cellmap_subscribe_diffs() {
        let map = CellMap::<String, i32>::new();
        map.insert("a".to_string(), 1);
        map.insert("b".to_string(), 2);

        let received = Arc::new(Mutex::new(Vec::new()));
        let received_clone = received.clone();

        let _guard = map.subscribe_diffs(move |diff| {
            received_clone.lock().unwrap().push(diff.clone());
        });

        // Should have received Initial with both entries
        let diffs = received.lock().unwrap();
        assert_eq!(diffs.len(), 1);
        match &diffs[0] {
            MapDiff::Initial { entries } => {
                assert_eq!(entries.len(), 2);
            }
            _ => panic!("Expected Initial diff"),
        }
        drop(diffs);

        // Insert should trigger diff
        map.insert("c".to_string(), 3);
        let diffs = received.lock().unwrap();
        assert_eq!(diffs.len(), 2);
        assert!(matches!(&diffs[1], MapDiff::Insert { key, value } if key == "c" && *value == 3));
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

    #[test]
    fn test_cellmap_select_initial() {
        let map = CellMap::<String, i32>::new();
        map.insert("a".to_string(), 5);
        map.insert("b".to_string(), 15);
        map.insert("c".to_string(), 25);

        let filtered = map.select(|v| *v > 10);

        // Should have only b and c
        assert_eq!(filtered.entries().get().len(), 2);
        assert!(filtered.contains_key(&"b".to_string()));
        assert!(filtered.contains_key(&"c".to_string()));
        assert!(!filtered.contains_key(&"a".to_string()));
    }

    #[test]
    fn test_cellmap_select_insert() {
        let map = CellMap::<String, i32>::new();
        let filtered = map.select(|v| *v > 10);

        assert_eq!(filtered.entries().get().len(), 0);

        // Insert non-matching
        map.insert("a".to_string(), 5);
        assert_eq!(filtered.entries().get().len(), 0);

        // Insert matching
        map.insert("b".to_string(), 15);
        assert_eq!(filtered.entries().get().len(), 1);
        assert!(filtered.contains_key(&"b".to_string()));
    }

    #[test]
    fn test_cellmap_select_remove() {
        let map = CellMap::<String, i32>::new();
        map.insert("a".to_string(), 15);
        map.insert("b".to_string(), 25);

        let filtered = map.select(|v| *v > 10);
        assert_eq!(filtered.entries().get().len(), 2);

        // Remove a matching item
        map.remove(&"a".to_string());
        assert_eq!(filtered.entries().get().len(), 1);
        assert!(!filtered.contains_key(&"a".to_string()));
        assert!(filtered.contains_key(&"b".to_string()));
    }

    #[test]
    fn test_cellmap_select_update_matching_to_non_matching() {
        let map = CellMap::<String, i32>::new();
        map.insert("a".to_string(), 15);

        let filtered = map.select(|v| *v > 10);
        assert_eq!(filtered.entries().get().len(), 1);

        // Update to non-matching value
        map.insert("a".to_string(), 5);
        assert_eq!(filtered.entries().get().len(), 0);
    }

    #[test]
    fn test_cellmap_select_update_non_matching_to_matching() {
        let map = CellMap::<String, i32>::new();
        map.insert("a".to_string(), 5);

        let filtered = map.select(|v| *v > 10);
        assert_eq!(filtered.entries().get().len(), 0);

        // Update to matching value
        map.insert("a".to_string(), 15);
        assert_eq!(filtered.entries().get().len(), 1);
        assert!(filtered.contains_key(&"a".to_string()));
    }

    #[test]
    fn test_cellmap_select_entries_observable() {
        let map = CellMap::<String, i32>::new();
        let filtered = map.select(|v| *v > 10);
        let entries = filtered.entries();

        let count = Arc::new(AtomicUsize::new(0));
        let c = count.clone();
        let _guard = entries.subscribe(move |_| {
            c.fetch_add(1, Ordering::SeqCst);
        });

        assert_eq!(count.load(Ordering::SeqCst), 1); // Initial

        // Insert matching
        map.insert("a".to_string(), 15);
        assert_eq!(count.load(Ordering::SeqCst), 2);

        // Insert non-matching - filtered should NOT change
        map.insert("b".to_string(), 5);
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn test_cellmap_select_on_locked_map() {
        let map = CellMap::<String, i32>::new();
        map.insert("a".to_string(), 5);
        map.insert("b".to_string(), 15);

        let locked = map.lock();
        let filtered = locked.select(|v| *v > 10);

        assert_eq!(filtered.entries().get().len(), 1);
        assert!(filtered.contains_key(&"b".to_string()));
    }

    /// Regression: select().entries().map() used as a temporary chain must propagate updates.
    /// The intermediate CellMap from select() is a temporary, but the entries Cell's callback
    /// keeps it alive via a captured Arc<CellMapInner>.
    #[test]
    fn test_select_entries_chain_propagates() {
        use crate::traits::MapExt;

        let store = CellMap::<String, i32>::new();
        store.insert("a".to_string(), 1);
        store.insert("b".to_string(), 20);

        // Chain without holding the intermediate CellMap (the exact pattern from queries)
        let result = store
            .select(|v| *v > 10)
            .entries()
            .map(|entries| entries.len());

        assert_eq!(result.get(), 1); // only "b"

        // Insert a new matching item — should propagate through the chain
        store.insert("c".to_string(), 30);
        assert_eq!(result.get(), 2); // "b" and "c"

        // Insert a non-matching item — should not change
        store.insert("d".to_string(), 5);
        assert_eq!(result.get(), 2);

        // Remove a matching item
        store.remove(&"b".to_string());
        assert_eq!(result.get(), 1); // only "c"
    }

    /// Verify that select() CellMap is eventually dropped when all downstream consumers are dropped.
    #[test]
    fn test_select_no_leak_when_dropped() {
        let store = CellMap::<String, i32>::new();
        store.insert("a".to_string(), 1);

        let initial_count = Arc::strong_count(&store.inner);

        {
            let _result = store.select(|v| *v > 0).entries();
            // While alive, the chain should work
            assert_eq!(_result.get().len(), 1);
        }
        // After dropping, the store's inner refcount should return to original
        // (the select subscription should be cleaned up)
        assert_eq!(Arc::strong_count(&store.inner), initial_count);
    }

    /// Regression: subscribe_diffs on a select()'d CellMap that's then dropped (passed by value).
    /// This is the exact pattern used in the WebSocket handler: the filtered CellMap is passed
    /// by value to subscribe_query, subscribe_diffs is called, then the CellMap goes out of scope.
    /// The subscription must still receive updates because subscribe_diffs captures a keepalive.
    #[test]
    fn test_subscribe_diffs_on_dropped_select() {
        let store = CellMap::<String, i32>::new();
        store.insert("a".to_string(), 1);
        store.insert("b".to_string(), 20);

        let received = Arc::new(Mutex::new(Vec::new()));
        let received_clone = received.clone();

        // Simulate the WebSocket handler pattern:
        // select() returns a temporary, subscribe_diffs is called, then CellMap is dropped
        let guard = store.select(|v| *v > 10).subscribe_diffs(move |diff| {
            if let MapDiff::Insert { key, .. } = diff {
                received_clone.lock().unwrap().push(key.clone());
            }
        });

        // Insert a new matching item after the filtered CellMap has been dropped
        store.insert("c".to_string(), 30);
        assert_eq!(received.lock().unwrap().as_slice(), &["c".to_string()]);

        // Non-matching should not appear
        store.insert("d".to_string(), 5);
        assert_eq!(received.lock().unwrap().len(), 1);

        drop(guard);
    }
}
