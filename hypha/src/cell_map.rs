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
    /// Multiple diffs emitted as a single notification.
    Batch { changes: Vec<MapDiff<K, V>> },
}

pub(crate) struct CellMapInner<K, V>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    /// The actual data storage.
    pub(crate) data: DashMap<K, V>,
    /// Cached per-key observation cells.
    key_cells: DashMap<K, WeakCell<Option<V>, CellMutable>>,
    /// Cell for diff notifications.
    pub(crate) diffs_cell: Cell<MapDiff<K, V>, CellMutable>,
    /// Cell for length.
    len_cell: Cell<usize, CellMutable>,
    /// Subscription guards owned by this map (dropped when map drops).
    owned: DashMap<Uuid, SubscriptionGuard>,
    /// Optional name for debugging.
    pub(crate) name: ArcSwap<Option<Arc<str>>>,
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
#[derive(Clone)]
pub struct CellMap<K, V, M = CellMutable>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    pub(crate) inner: Arc<CellMapInner<K, V>>,
    pub(crate) _marker: PhantomData<M>,
}

/// Weak handle for a `CellMap`.
///
/// This allows callbacks to reference a map without retaining it strongly,
/// which helps avoid reference cycles in subscription graphs.
pub struct WeakCellMap<K, V>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    inner: std::sync::Weak<CellMapInner<K, V>>,
}

impl<K, V> CellMap<K, V, CellMutable>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    /// Create a new empty CellMap.
    #[track_caller]
    pub fn new() -> Self {
        let diffs_cell = Cell::new(MapDiff::Initial {
            entries: Vec::new(),
        });
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

    /// Own a subscription guard, keeping it alive as long as this CellMap exists.
    ///
    /// This enables building custom reactive CellMaps driven by external cells.
    pub fn own_guard(&self, guard: SubscriptionGuard) {
        self.own(guard);
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
        self.inner.diffs_cell.set(diff);

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

    /// Insert multiple key-value pairs and emit a single batch diff.
    pub fn insert_many(&self, entries: Vec<(K, V)>) {
        if entries.is_empty() {
            return;
        }

        let mut changes = Vec::with_capacity(entries.len());
        for (key, value) in entries {
            let old = self.inner.data.insert(key.clone(), value.clone());
            let diff = match old {
                Some(old_value) => MapDiff::Update {
                    key: key.clone(),
                    old_value,
                    new_value: value.clone(),
                },
                None => MapDiff::Insert {
                    key: key.clone(),
                    value: value.clone(),
                },
            };
            changes.push(diff);

            if let Some(weak) = self.inner.key_cells.get(&key)
                && let Some(cell) = weak.upgrade()
            {
                cell.set(Some(value));
            }
        }

        self.inner.diffs_cell.set(MapDiff::Batch { changes });
        self.inner.len_cell.set(self.inner.data.len());
    }

    /// Remove a key, returning the old value if present.
    pub fn remove(&self, key: &K) -> Option<V> {
        let removed = self.inner.data.remove(key);

        if let Some((k, old_value)) = removed {
            // Emit diff (O(1) - just notifies subscribers)
            self.inner.diffs_cell.set(MapDiff::Remove {
                key: k.clone(),
                old_value: old_value.clone(),
            });

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

    /// Remove multiple keys and emit a single batch diff.
    pub fn remove_many(&self, keys: Vec<K>) {
        if keys.is_empty() {
            return;
        }

        let mut changes = Vec::new();
        for key in keys {
            let removed = self.inner.data.remove(&key);
            if let Some((k, old_value)) = removed {
                changes.push(MapDiff::Remove {
                    key: k.clone(),
                    old_value: old_value.clone(),
                });

                if let Some(weak) = self.inner.key_cells.get(&k)
                    && let Some(cell) = weak.upgrade()
                {
                    cell.set(None);
                }
            }
        }

        if changes.is_empty() {
            return;
        }

        self.inner.diffs_cell.set(MapDiff::Batch { changes });
        self.inner.len_cell.set(self.inner.data.len());
    }

    /// Apply a batch of diffs and emit them as one `MapDiff::Batch`.
    pub fn apply_batch(&self, changes: Vec<MapDiff<K, V>>) {
        if changes.is_empty() {
            return;
        }

        fn apply_one<K, V>(map: &CellMap<K, V, CellMutable>, diff: &MapDiff<K, V>)
        where
            K: Hash + Eq + CellValue,
            V: CellValue,
        {
            match diff {
                MapDiff::Initial { entries } => {
                    let keys: Vec<K> = map.inner.data.iter().map(|r| r.key().clone()).collect();
                    for key in keys {
                        map.inner.data.remove(&key);
                        if let Some(weak) = map.inner.key_cells.get(&key)
                            && let Some(cell) = weak.upgrade()
                        {
                            cell.set(None);
                        }
                    }

                    for (key, value) in entries {
                        map.inner.data.insert(key.clone(), value.clone());
                        if let Some(weak) = map.inner.key_cells.get(key)
                            && let Some(cell) = weak.upgrade()
                        {
                            cell.set(Some(value.clone()));
                        }
                    }
                }
                MapDiff::Insert { key, value } => {
                    map.inner.data.insert(key.clone(), value.clone());
                    if let Some(weak) = map.inner.key_cells.get(key)
                        && let Some(cell) = weak.upgrade()
                    {
                        cell.set(Some(value.clone()));
                    }
                }
                MapDiff::Remove { key, .. } => {
                    map.inner.data.remove(key);
                    if let Some(weak) = map.inner.key_cells.get(key)
                        && let Some(cell) = weak.upgrade()
                    {
                        cell.set(None);
                    }
                }
                MapDiff::Update { key, new_value, .. } => {
                    map.inner.data.insert(key.clone(), new_value.clone());
                    if let Some(weak) = map.inner.key_cells.get(key)
                        && let Some(cell) = weak.upgrade()
                    {
                        cell.set(Some(new_value.clone()));
                    }
                }
                MapDiff::Batch { changes } => {
                    for change in changes {
                        apply_one(map, change);
                    }
                }
            }
        }

        for change in &changes {
            apply_one(self, change);
        }

        self.inner.len_cell.set(self.inner.data.len());
        self.inner.diffs_cell.set(MapDiff::Batch { changes });
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
    /// Create a weak handle to this map.
    pub fn downgrade(&self) -> WeakCellMap<K, V> {
        WeakCellMap {
            inner: Arc::downgrade(&self.inner),
        }
    }

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
            if let Signal::Value(diff) = signal {
                // Apply diff incrementally to the entries
                let mut entries = cell.get();
                fn apply_diff<K, V>(entries: &mut Vec<(K, V)>, diff: &MapDiff<K, V>)
                where
                    K: Hash + Eq + CellValue,
                    V: CellValue,
                {
                    match diff {
                        MapDiff::Initial { entries: init } => {
                            *entries = init.clone();
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
                        MapDiff::Batch { changes } => {
                            for change in changes {
                                apply_diff(entries, change);
                            }
                        }
                    }
                }

                apply_diff(&mut entries, diff.as_ref());
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
    /// Emits `MapDiff` updates. Starts with `MapDiff::Initial { entries: vec![] }`.
    pub fn diffs(&self) -> Cell<MapDiff<K, V>, CellImmutable> {
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
            if let crate::Signal::Value(diff) = signal {
                callback(diff.as_ref());
            }
        })
    }
}

impl<K, V> WeakCellMap<K, V>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    /// Upgrade to a mutable `CellMap` if it is still alive.
    pub fn upgrade(&self) -> Option<CellMap<K, V, CellMutable>> {
        self.inner.upgrade().map(|inner| CellMap {
            inner,
            _marker: PhantomData,
        })
    }
}

// ── ReactiveKeys / ReactiveMap impl ─────────────────────────────────────

use crate::traits::{
    reactive_keys::{KeyChange, ReactiveKeys},
    reactive_map::ReactiveMap,
};

/// Convert a `MapDiff` into its `KeyChange` equivalent.
/// Returns `None` for `Update` (key unchanged — no membership change).
pub(crate) fn map_diff_to_key_change<K: CellValue, V: CellValue>(
    diff: &MapDiff<K, V>,
) -> Option<KeyChange<K>> {
    match diff {
        MapDiff::Initial { entries } => Some(KeyChange::Initial(
            entries.iter().map(|(k, _)| k.clone()).collect(),
        )),
        MapDiff::Insert { key, .. } => Some(KeyChange::Added(key.clone())),
        MapDiff::Remove { key, .. } => Some(KeyChange::Removed(key.clone())),
        MapDiff::Update { .. } => None,
        MapDiff::Batch { changes } => {
            let key_changes: Vec<KeyChange<K>> =
                changes.iter().filter_map(map_diff_to_key_change).collect();
            if key_changes.is_empty() {
                None
            } else {
                Some(KeyChange::Batch(key_changes))
            }
        }
    }
}

impl<K, V, M> ReactiveKeys for CellMap<K, V, M>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
    M: Send + Sync + 'static,
{
    type Key = K;

    fn key_set(&self) -> Vec<K> {
        self.inner.data.iter().map(|r| r.key().clone()).collect()
    }

    fn contains_key(&self, key: &K) -> bool {
        self.inner.data.contains_key(key)
    }

    fn subscribe_keys(
        &self,
        cb: impl Fn(&KeyChange<K>) + Send + Sync + 'static,
    ) -> SubscriptionGuard {
        self.subscribe_diffs(move |diff| {
            if let Some(kc) = map_diff_to_key_change(diff) {
                cb(&kc);
            }
        })
    }
}

impl<K, V, M> ReactiveMap for CellMap<K, V, M>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
    M: Send + Sync + 'static,
{
    type Value = V;

    fn get_value(&self, key: &K) -> Option<V> {
        self.inner.data.get(key).map(|r| r.value().clone())
    }

    fn snapshot(&self) -> Vec<(K, V)> {
        self.inner
            .data
            .iter()
            .map(|r| (r.key().clone(), r.value().clone()))
            .collect()
    }

    fn subscribe_diffs_reactive(
        &self,
        cb: impl Fn(&MapDiff<K, V>) + Send + Sync + 'static,
    ) -> SubscriptionGuard {
        self.subscribe_diffs(cb)
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

        assert_eq!(diffs.get(), MapDiff::Initial { entries: vec![] });

        map.insert("a".to_string(), 1);
        assert_eq!(
            diffs.get(),
            MapDiff::Insert {
                key: "a".to_string(),
                value: 1
            }
        );

        map.insert("a".to_string(), 2);
        assert_eq!(
            diffs.get(),
            MapDiff::Update {
                key: "a".to_string(),
                old_value: 1,
                new_value: 2
            }
        );

        map.remove(&"a".to_string());
        assert_eq!(
            diffs.get(),
            MapDiff::Remove {
                key: "a".to_string(),
                old_value: 2
            }
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
    fn test_apply_batch_emits_single_batch_diff() {
        let map = CellMap::<String, i32>::new();
        let (tx, rx) = std::sync::mpsc::channel::<MapDiff<String, i32>>();

        let _guard = map.subscribe_diffs(move |diff| {
            let _ = tx.send(diff.clone());
        });

        map.apply_batch(vec![
            MapDiff::Insert {
                key: "a".to_string(),
                value: 1,
            },
            MapDiff::Insert {
                key: "b".to_string(),
                value: 2,
            },
        ]);

        let seen: Vec<_> = rx.try_iter().collect();
        assert_eq!(seen.len(), 2);
        match &seen[0] {
            MapDiff::Initial { entries } => assert!(entries.is_empty()),
            _ => panic!("expected Initial diff first"),
        }
        match &seen[1] {
            MapDiff::Batch { changes } => assert_eq!(changes.len(), 2),
            _ => panic!("expected single Batch diff from apply_batch"),
        }
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
    fn test_cellmap_mutable_clone_shares_inner_map() {
        let map = CellMap::<String, i32>::new();
        let map_clone = map.clone();

        map.insert("a".to_string(), 1);
        assert_eq!(map_clone.get_value(&"a".to_string()), Some(1));

        map_clone.insert("b".to_string(), 2);
        assert_eq!(map.get_value(&"b".to_string()), Some(2));
        assert_eq!(map.len().get(), 2);
        assert_eq!(map_clone.len().get(), 2);
    }
}
