//! Reactive HashMap with per-key observability.
//!
//! `CellMap` wraps a concurrent HashMap where each entry can be individually observed.
//! Changes to keys trigger reactive updates to observers.

use std::{
    collections::{BTreeMap, HashSet},
    hash::Hash,
    marker::PhantomData,
    sync::Arc,
};

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
    diffs_cell: Cell<MapDiff<K, V>, CellMutable>,
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
#[derive(Clone)]
pub struct CellMap<K, V, M = CellMutable>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    inner: Arc<CellMapInner<K, V>>,
    _marker: PhantomData<M>,
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

    /// Create a filtered view of this CellMap using a reactive per-entry predicate.
    ///
    /// The predicate returns a bool-producing cell for each `(key, value)` pair.
    /// Membership in the filtered map updates when either:
    /// - source map rows are inserted/updated/removed
    /// - the predicate cell for a row changes
    #[track_caller]
    fn select_cell<W, F>(&self, predicate: F) -> CellMap<K, V, CellImmutable>
    where
        W: Watchable<bool> + Gettable<bool> + Clone + Send + Sync + 'static,
        F: Fn(&K, &V) -> W + Send + Sync + 'static;

    /// Map each row to a row-local cell and keep output rows in sync with those cells.
    ///
    /// Similar to `switch_map`, but applied per key in the source map:
    /// - insert/update source row: (re)subscribe that row's inner cell
    /// - remove source row: unsubscribe that row and remove output row
    /// - inner row cell emits: update only that output row
    #[track_caller]
    fn switch_map_cell<U, W, F>(&self, mapper: F) -> CellMap<K, U, CellImmutable>
    where
        U: CellValue,
        W: Watchable<U> + Gettable<U> + Clone + Send + Sync + 'static,
        F: Fn(&K, &V) -> W + Send + Sync + 'static;

    /// Filter rows using a lookup map keyed by a derived key from each source row.
    #[track_caller]
    fn select_lookup<LK, R, LM, FK, FP>(
        &self,
        lookup: &CellMap<LK, R, LM>,
        lookup_key: FK,
        predicate: FP,
    ) -> CellMap<K, V, CellImmutable>
    where
        LK: Hash + Eq + CellValue,
        R: CellValue,
        FK: Fn(&K, &V) -> LK + Send + Sync + 'static,
        FP: Fn(&K, &V, Option<&R>) -> bool + Send + Sync + 'static;

    /// Map rows using a lookup map keyed by a derived key from each source row.
    #[track_caller]
    fn switch_map_lookup<U, LK, R, LM, FK, FM>(
        &self,
        lookup: &CellMap<LK, R, LM>,
        lookup_key: FK,
        mapper: FM,
    ) -> CellMap<K, U, CellImmutable>
    where
        U: CellValue,
        LK: Hash + Eq + CellValue,
        R: CellValue,
        FK: Fn(&K, &V) -> LK + Send + Sync + 'static,
        FM: Fn(&K, &V, Option<&R>) -> U + Send + Sync + 'static;

    /// Count source rows grouped by a derived key.
    #[track_caller]
    fn count_by<GK, F>(&self, group_key: F) -> CellMap<GK, usize, CellImmutable>
    where
        GK: Hash + Eq + CellValue,
        F: Fn(&K, &V) -> GK + Send + Sync + 'static;

    /// Group source rows by a derived key, emitting grouped row values per group.
    #[track_caller]
    fn group_by<GK, F>(&self, group_key: F) -> CellMap<GK, Vec<V>, CellImmutable>
    where
        K: Ord,
        GK: Hash + Eq + CellValue,
        F: Fn(&K, &V) -> GK + Send + Sync + 'static;
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
                MapDiff::Batch { changes } => {
                    for change in changes {
                        match change {
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
                            MapDiff::Batch { .. } => {}
                        }
                    }
                }
            }
        });

        // Own the guard so it stays alive as long as the filtered map exists
        filtered.own(guard);

        // Return locked (immutable) view
        filtered.lock()
    }

    #[track_caller]
    fn select_cell<W, F>(&self, predicate: F) -> CellMap<K, V, CellImmutable>
    where
        W: Watchable<bool> + Gettable<bool> + Clone + Send + Sync + 'static,
        F: Fn(&K, &V) -> W + Send + Sync + 'static,
    {
        let filtered = CellMap::<K, V, CellMutable>::new();
        if let Some(parent_name) = (**self.inner.name.load()).as_ref() {
            filtered
                .clone()
                .with_name(format!("{}::select_cell", parent_name));
        }

        let predicate = Arc::new(predicate);
        let per_key_guards = Arc::new(DashMap::<K, SubscriptionGuard>::new());

        let filtered_weak = Arc::downgrade(&filtered.inner);
        let guard = self.subscribe_diffs(move |diff| {
            let Some(inner) = filtered_weak.upgrade() else {
                return;
            };
            let filtered = CellMap::<K, V, CellMutable> {
                inner,
                _marker: PhantomData,
            };

            let attach = |key: K, value: V, mut batch_changes: Option<&mut Vec<MapDiff<K, V>>>| {
                if let Some((_, old_guard)) = per_key_guards.remove(&key) {
                    drop(old_guard);
                }

                let include_cell = predicate(&key, &value);
                let suppress_initial = batch_changes.is_some();
                if let Some(changes) = batch_changes.as_deref_mut() {
                    let include = include_cell.get();
                    let current = filtered.get_value(&key);
                    if include {
                        if let Some(old_value) = current {
                            changes.push(MapDiff::Update {
                                key: key.clone(),
                                old_value,
                                new_value: value.clone(),
                            });
                        } else {
                            changes.push(MapDiff::Insert {
                                key: key.clone(),
                                value: value.clone(),
                            });
                        }
                    } else if let Some(old_value) = current {
                        changes.push(MapDiff::Remove {
                            key: key.clone(),
                            old_value,
                        });
                    }
                }

                let key_for_sub = key.clone();
                let value_for_sub = value.clone();
                let filtered_weak_for_sub = Arc::downgrade(&filtered.inner);
                let first = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
                let first_for_sub = first.clone();

                let sub_guard = include_cell.subscribe(move |include_signal| {
                    let Some(inner) = filtered_weak_for_sub.upgrade() else {
                        return;
                    };
                    let filtered = CellMap::<K, V, CellMutable> {
                        inner,
                        _marker: PhantomData,
                    };

                    if let Signal::Value(include) = include_signal {
                        if suppress_initial
                            && first_for_sub.swap(false, std::sync::atomic::Ordering::SeqCst)
                        {
                            return;
                        }
                        if **include {
                            filtered.insert(key_for_sub.clone(), value_for_sub.clone());
                        } else {
                            filtered.remove(&key_for_sub);
                        }
                    }
                });

                per_key_guards.insert(key, sub_guard);
            };

            let detach = |key: &K, batch_changes: Option<&mut Vec<MapDiff<K, V>>>| {
                if let Some((_, old_guard)) = per_key_guards.remove(key) {
                    drop(old_guard);
                }
                if let Some(changes) = batch_changes {
                    if let Some(old_value) = filtered.get_value(key) {
                        changes.push(MapDiff::Remove {
                            key: key.clone(),
                            old_value,
                        });
                    }
                } else {
                    filtered.remove(key);
                }
            };

            match diff {
                MapDiff::Initial { entries } => {
                    for (key, value) in entries {
                        attach(key.clone(), value.clone(), None);
                    }
                }
                MapDiff::Insert { key, value } => {
                    attach(key.clone(), value.clone(), None);
                }
                MapDiff::Remove { key, .. } => {
                    detach(key, None);
                }
                MapDiff::Update { key, new_value, .. } => {
                    attach(key.clone(), new_value.clone(), None);
                }
                MapDiff::Batch { changes } => {
                    let mut downstream_changes: Vec<MapDiff<K, V>> = Vec::new();
                    for change in changes {
                        match change {
                            MapDiff::Initial { entries } => {
                                let existing_keys: Vec<K> = per_key_guards
                                    .iter()
                                    .map(|entry| entry.key().clone())
                                    .collect();
                                for k in existing_keys {
                                    detach(&k, Some(&mut downstream_changes));
                                }
                                for (key, value) in entries {
                                    attach(
                                        key.clone(),
                                        value.clone(),
                                        Some(&mut downstream_changes),
                                    );
                                }
                            }
                            MapDiff::Insert { key, value } => {
                                attach(key.clone(), value.clone(), Some(&mut downstream_changes));
                            }
                            MapDiff::Remove { key, .. } => {
                                detach(key, Some(&mut downstream_changes));
                            }
                            MapDiff::Update { key, new_value, .. } => {
                                attach(
                                    key.clone(),
                                    new_value.clone(),
                                    Some(&mut downstream_changes),
                                );
                            }
                            MapDiff::Batch { .. } => {}
                        }
                    }
                    filtered.apply_batch(downstream_changes);
                }
            }
        });

        filtered.own(guard);
        filtered.lock()
    }

    #[track_caller]
    fn switch_map_cell<U, W, F>(&self, mapper: F) -> CellMap<K, U, CellImmutable>
    where
        U: CellValue,
        W: Watchable<U> + Gettable<U> + Clone + Send + Sync + 'static,
        F: Fn(&K, &V) -> W + Send + Sync + 'static,
    {
        let mapped = CellMap::<K, U, CellMutable>::new();
        if let Some(parent_name) = (**self.inner.name.load()).as_ref() {
            mapped
                .clone()
                .with_name(format!("{}::switch_map_cell", parent_name));
        }

        let mapper = Arc::new(mapper);
        let per_key_guards = Arc::new(DashMap::<K, SubscriptionGuard>::new());
        let mapped_weak = Arc::downgrade(&mapped.inner);

        let guard = self.subscribe_diffs(move |diff| {
            let Some(inner) = mapped_weak.upgrade() else {
                return;
            };
            let mapped = CellMap::<K, U, CellMutable> {
                inner,
                _marker: PhantomData,
            };

            let attach = |key: K, value: V, mut batch_changes: Option<&mut Vec<MapDiff<K, U>>>| {
                if let Some((_, old_guard)) = per_key_guards.remove(&key) {
                    drop(old_guard);
                }

                let inner_cell = mapper(&key, &value);
                let suppress_initial = batch_changes.is_some();
                if let Some(changes) = batch_changes.as_deref_mut() {
                    let initial = inner_cell.get();
                    if let Some(old_value) = mapped.get_value(&key) {
                        changes.push(MapDiff::Update {
                            key: key.clone(),
                            old_value,
                            new_value: initial,
                        });
                    } else {
                        changes.push(MapDiff::Insert {
                            key: key.clone(),
                            value: initial,
                        });
                    }
                }
                let key_for_sub = key.clone();
                let mapped_weak_for_sub = Arc::downgrade(&mapped.inner);
                let first = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
                let first_for_sub = first.clone();
                let sub_guard = inner_cell.subscribe(move |signal| {
                    let Some(inner) = mapped_weak_for_sub.upgrade() else {
                        return;
                    };
                    let mapped = CellMap::<K, U, CellMutable> {
                        inner,
                        _marker: PhantomData,
                    };

                    if let Signal::Value(v) = signal {
                        if suppress_initial
                            && first_for_sub.swap(false, std::sync::atomic::Ordering::SeqCst)
                        {
                            return;
                        }
                        mapped.insert(key_for_sub.clone(), (**v).clone());
                    }
                });
                per_key_guards.insert(key, sub_guard);
            };

            let detach = |key: &K, batch_changes: Option<&mut Vec<MapDiff<K, U>>>| {
                if let Some((_, old_guard)) = per_key_guards.remove(key) {
                    drop(old_guard);
                }
                if let Some(changes) = batch_changes {
                    if let Some(old_value) = mapped.get_value(key) {
                        changes.push(MapDiff::Remove {
                            key: key.clone(),
                            old_value,
                        });
                    }
                } else {
                    mapped.remove(key);
                }
            };

            match diff {
                MapDiff::Initial { entries } => {
                    let existing_keys: Vec<K> = per_key_guards
                        .iter()
                        .map(|entry| entry.key().clone())
                        .collect();
                    for k in existing_keys {
                        detach(&k, None);
                    }
                    for (key, value) in entries {
                        attach(key.clone(), value.clone(), None);
                    }
                }
                MapDiff::Insert { key, value } => {
                    attach(key.clone(), value.clone(), None);
                }
                MapDiff::Remove { key, .. } => {
                    detach(key, None);
                }
                MapDiff::Update { key, new_value, .. } => {
                    attach(key.clone(), new_value.clone(), None);
                }
                MapDiff::Batch { changes } => {
                    let mut downstream_changes: Vec<MapDiff<K, U>> = Vec::new();
                    for change in changes {
                        match change {
                            MapDiff::Initial { entries } => {
                                let existing_keys: Vec<K> = per_key_guards
                                    .iter()
                                    .map(|entry| entry.key().clone())
                                    .collect();
                                for k in existing_keys {
                                    detach(&k, Some(&mut downstream_changes));
                                }
                                for (key, value) in entries {
                                    attach(
                                        key.clone(),
                                        value.clone(),
                                        Some(&mut downstream_changes),
                                    );
                                }
                            }
                            MapDiff::Insert { key, value } => {
                                attach(key.clone(), value.clone(), Some(&mut downstream_changes));
                            }
                            MapDiff::Remove { key, .. } => {
                                detach(key, Some(&mut downstream_changes));
                            }
                            MapDiff::Update { key, new_value, .. } => {
                                attach(
                                    key.clone(),
                                    new_value.clone(),
                                    Some(&mut downstream_changes),
                                );
                            }
                            MapDiff::Batch { .. } => {}
                        }
                    }
                    mapped.apply_batch(downstream_changes);
                }
            }
        });

        mapped.own(guard);
        mapped.lock()
    }

    #[track_caller]
    fn select_lookup<LK, R, LM, FK, FP>(
        &self,
        lookup: &CellMap<LK, R, LM>,
        lookup_key: FK,
        predicate: FP,
    ) -> CellMap<K, V, CellImmutable>
    where
        LK: Hash + Eq + CellValue,
        R: CellValue,
        FK: Fn(&K, &V) -> LK + Send + Sync + 'static,
        FP: Fn(&K, &V, Option<&R>) -> bool + Send + Sync + 'static,
    {
        let output = CellMap::<K, V, CellMutable>::new();
        if let Some(parent_name) = (**self.inner.name.load()).as_ref() {
            output
                .clone()
                .with_name(format!("{}::select_lookup", parent_name));
        }

        let lookup_key = Arc::new(lookup_key);
        let predicate = Arc::new(predicate);
        let left_shadow = Arc::new(DashMap::<K, V>::new());
        let left_lookup_keys = Arc::new(DashMap::<K, LK>::new());
        let lookup_shadow = Arc::new(DashMap::<LK, R>::new());

        let output_weak = Arc::downgrade(&output.inner);

        let left_guard = self.subscribe_diffs({
            let lookup_key = lookup_key.clone();
            let predicate = predicate.clone();
            let left_shadow = left_shadow.clone();
            let left_lookup_keys = left_lookup_keys.clone();
            let lookup_shadow = lookup_shadow.clone();
            move |diff| {
                let Some(inner) = output_weak.upgrade() else {
                    return;
                };
                let output = CellMap::<K, V, CellMutable> {
                    inner,
                    _marker: PhantomData,
                };

                fn apply_left<K, V, LK>(
                    diff: &MapDiff<K, V>,
                    left_shadow: &DashMap<K, V>,
                    left_lookup_keys: &DashMap<K, LK>,
                    lookup_key: &Arc<dyn Fn(&K, &V) -> LK + Send + Sync>,
                    impacted: &mut HashSet<K>,
                ) where
                    K: Hash + Eq + CellValue,
                    V: CellValue,
                    LK: Hash + Eq + CellValue,
                {
                    match diff {
                        MapDiff::Initial { entries } => {
                            let previous: Vec<K> = left_shadow
                                .iter()
                                .map(|entry| entry.key().clone())
                                .collect();
                            left_shadow.clear();
                            left_lookup_keys.clear();
                            for key in previous {
                                impacted.insert(key);
                            }
                            for (k, v) in entries {
                                left_shadow.insert(k.clone(), v.clone());
                                left_lookup_keys.insert(k.clone(), lookup_key(k, v));
                                impacted.insert(k.clone());
                            }
                        }
                        MapDiff::Insert { key, value }
                        | MapDiff::Update {
                            key,
                            new_value: value,
                            ..
                        } => {
                            left_shadow.insert(key.clone(), value.clone());
                            left_lookup_keys.insert(key.clone(), lookup_key(key, value));
                            impacted.insert(key.clone());
                        }
                        MapDiff::Remove { key, .. } => {
                            left_shadow.remove(key);
                            left_lookup_keys.remove(key);
                            impacted.insert(key.clone());
                        }
                        MapDiff::Batch { changes } => {
                            for change in changes {
                                apply_left(
                                    change,
                                    left_shadow,
                                    left_lookup_keys,
                                    lookup_key,
                                    impacted,
                                );
                            }
                        }
                    }
                }

                let lookup_key_dyn: Arc<dyn Fn(&K, &V) -> LK + Send + Sync> = lookup_key.clone();
                let mut impacted: HashSet<K> = HashSet::new();
                apply_left(
                    diff,
                    &left_shadow,
                    &left_lookup_keys,
                    &lookup_key_dyn,
                    &mut impacted,
                );
                log::trace!(
                    target: "hypha::cell_map::select_lookup",
                    "left diff processed: impacted_keys={}",
                    impacted.len()
                );

                let mut changes: Vec<MapDiff<K, V>> = Vec::new();
                for key in impacted {
                    let desired = left_shadow.get(&key).and_then(|left_value| {
                        let lk = left_lookup_keys
                            .get(&key)
                            .map(|entry| entry.value().clone())
                            .unwrap_or_else(|| lookup_key(&key, left_value.value()));
                        let right = lookup_shadow.get(&lk);
                        let include =
                            predicate(&key, left_value.value(), right.as_ref().map(|r| r.value()));
                        if include {
                            Some(left_value.value().clone())
                        } else {
                            None
                        }
                    });

                    match (output.get_value(&key), desired) {
                        (Some(old_value), Some(new_value)) => changes.push(MapDiff::Update {
                            key: key.clone(),
                            old_value,
                            new_value,
                        }),
                        (None, Some(new_value)) => changes.push(MapDiff::Insert {
                            key: key.clone(),
                            value: new_value,
                        }),
                        (Some(old_value), None) => changes.push(MapDiff::Remove {
                            key: key.clone(),
                            old_value,
                        }),
                        (None, None) => {}
                    }
                }
                log::trace!(
                    target: "hypha::cell_map::select_lookup",
                    "left recompute emitted_changes={}",
                    changes.len()
                );
                output.apply_batch(changes);
            }
        });

        let output_weak = Arc::downgrade(&output.inner);
        let lookup_guard = lookup.subscribe_diffs({
            let lookup_key = lookup_key.clone();
            let predicate = predicate.clone();
            let left_shadow = left_shadow.clone();
            let left_lookup_keys = left_lookup_keys.clone();
            let lookup_shadow = lookup_shadow.clone();
            move |diff| {
                let Some(inner) = output_weak.upgrade() else {
                    return;
                };
                let output = CellMap::<K, V, CellMutable> {
                    inner,
                    _marker: PhantomData,
                };

                fn apply_lookup<LK, R>(
                    diff: &MapDiff<LK, R>,
                    lookup_shadow: &DashMap<LK, R>,
                    changed_lookup: &mut HashSet<LK>,
                ) where
                    LK: Hash + Eq + CellValue,
                    R: CellValue,
                {
                    match diff {
                        MapDiff::Initial { entries } => {
                            let previous: Vec<LK> = lookup_shadow
                                .iter()
                                .map(|entry| entry.key().clone())
                                .collect();
                            for k in previous {
                                changed_lookup.insert(k);
                            }
                            lookup_shadow.clear();
                            for (k, v) in entries {
                                lookup_shadow.insert(k.clone(), v.clone());
                                changed_lookup.insert(k.clone());
                            }
                        }
                        MapDiff::Insert { key, value }
                        | MapDiff::Update {
                            key,
                            new_value: value,
                            ..
                        } => {
                            lookup_shadow.insert(key.clone(), value.clone());
                            changed_lookup.insert(key.clone());
                        }
                        MapDiff::Remove { key, .. } => {
                            lookup_shadow.remove(key);
                            changed_lookup.insert(key.clone());
                        }
                        MapDiff::Batch { changes } => {
                            for change in changes {
                                apply_lookup(change, lookup_shadow, changed_lookup);
                            }
                        }
                    }
                }

                let mut changed_lookup: HashSet<LK> = HashSet::new();
                apply_lookup(diff, &lookup_shadow, &mut changed_lookup);
                if changed_lookup.is_empty() {
                    log::trace!(
                        target: "hypha::cell_map::select_lookup",
                        "lookup diff processed: changed_lookup=0"
                    );
                    return;
                }

                let impacted: Vec<K> = left_lookup_keys
                    .iter()
                    .filter(|entry| changed_lookup.contains(entry.value()))
                    .map(|entry| entry.key().clone())
                    .collect();
                log::trace!(
                    target: "hypha::cell_map::select_lookup",
                    "lookup diff processed: changed_lookup={} impacted_keys={}",
                    changed_lookup.len(),
                    impacted.len()
                );

                let mut changes: Vec<MapDiff<K, V>> = Vec::new();
                for key in impacted {
                    let desired = left_shadow.get(&key).and_then(|left_value| {
                        let lk = left_lookup_keys
                            .get(&key)
                            .map(|entry| entry.value().clone())
                            .unwrap_or_else(|| lookup_key(&key, left_value.value()));
                        let right = lookup_shadow.get(&lk);
                        let include =
                            predicate(&key, left_value.value(), right.as_ref().map(|r| r.value()));
                        if include {
                            Some(left_value.value().clone())
                        } else {
                            None
                        }
                    });

                    match (output.get_value(&key), desired) {
                        (Some(old_value), Some(new_value)) => changes.push(MapDiff::Update {
                            key: key.clone(),
                            old_value,
                            new_value,
                        }),
                        (None, Some(new_value)) => changes.push(MapDiff::Insert {
                            key: key.clone(),
                            value: new_value,
                        }),
                        (Some(old_value), None) => changes.push(MapDiff::Remove {
                            key: key.clone(),
                            old_value,
                        }),
                        (None, None) => {}
                    }
                }
                log::trace!(
                    target: "hypha::cell_map::select_lookup",
                    "lookup recompute emitted_changes={}",
                    changes.len()
                );
                output.apply_batch(changes);
            }
        });

        output.own(left_guard);
        output.own(lookup_guard);
        output.lock()
    }

    #[track_caller]
    fn switch_map_lookup<U, LK, R, LM, FK, FM>(
        &self,
        lookup: &CellMap<LK, R, LM>,
        lookup_key: FK,
        mapper: FM,
    ) -> CellMap<K, U, CellImmutable>
    where
        U: CellValue,
        LK: Hash + Eq + CellValue,
        R: CellValue,
        FK: Fn(&K, &V) -> LK + Send + Sync + 'static,
        FM: Fn(&K, &V, Option<&R>) -> U + Send + Sync + 'static,
    {
        let output = CellMap::<K, U, CellMutable>::new();
        if let Some(parent_name) = (**self.inner.name.load()).as_ref() {
            output
                .clone()
                .with_name(format!("{}::switch_map_lookup", parent_name));
        }

        let lookup_key = Arc::new(lookup_key);
        let mapper = Arc::new(mapper);
        let left_shadow = Arc::new(DashMap::<K, V>::new());
        let left_lookup_keys = Arc::new(DashMap::<K, LK>::new());
        let lookup_shadow = Arc::new(DashMap::<LK, R>::new());

        let output_weak = Arc::downgrade(&output.inner);
        let left_guard = self.subscribe_diffs({
            let lookup_key = lookup_key.clone();
            let mapper = mapper.clone();
            let left_shadow = left_shadow.clone();
            let left_lookup_keys = left_lookup_keys.clone();
            let lookup_shadow = lookup_shadow.clone();
            move |diff| {
                let Some(inner) = output_weak.upgrade() else {
                    return;
                };
                let output = CellMap::<K, U, CellMutable> {
                    inner,
                    _marker: PhantomData,
                };

                fn apply_left<K, V, LK>(
                    diff: &MapDiff<K, V>,
                    left_shadow: &DashMap<K, V>,
                    left_lookup_keys: &DashMap<K, LK>,
                    lookup_key: &Arc<dyn Fn(&K, &V) -> LK + Send + Sync>,
                    impacted: &mut HashSet<K>,
                ) where
                    K: Hash + Eq + CellValue,
                    V: CellValue,
                    LK: Hash + Eq + CellValue,
                {
                    match diff {
                        MapDiff::Initial { entries } => {
                            let previous: Vec<K> = left_shadow
                                .iter()
                                .map(|entry| entry.key().clone())
                                .collect();
                            left_shadow.clear();
                            left_lookup_keys.clear();
                            for key in previous {
                                impacted.insert(key);
                            }
                            for (k, v) in entries {
                                left_shadow.insert(k.clone(), v.clone());
                                left_lookup_keys.insert(k.clone(), lookup_key(k, v));
                                impacted.insert(k.clone());
                            }
                        }
                        MapDiff::Insert { key, value }
                        | MapDiff::Update {
                            key,
                            new_value: value,
                            ..
                        } => {
                            left_shadow.insert(key.clone(), value.clone());
                            left_lookup_keys.insert(key.clone(), lookup_key(key, value));
                            impacted.insert(key.clone());
                        }
                        MapDiff::Remove { key, .. } => {
                            left_shadow.remove(key);
                            left_lookup_keys.remove(key);
                            impacted.insert(key.clone());
                        }
                        MapDiff::Batch { changes } => {
                            for change in changes {
                                apply_left(
                                    change,
                                    left_shadow,
                                    left_lookup_keys,
                                    lookup_key,
                                    impacted,
                                );
                            }
                        }
                    }
                }

                let lookup_key_dyn: Arc<dyn Fn(&K, &V) -> LK + Send + Sync> = lookup_key.clone();
                let mut impacted: HashSet<K> = HashSet::new();
                apply_left(
                    diff,
                    &left_shadow,
                    &left_lookup_keys,
                    &lookup_key_dyn,
                    &mut impacted,
                );
                log::trace!(
                    target: "hypha::cell_map::switch_map_lookup",
                    "left diff processed: impacted_keys={}",
                    impacted.len()
                );

                let mut changes: Vec<MapDiff<K, U>> = Vec::new();
                for key in impacted {
                    let desired = left_shadow.get(&key).map(|left_value| {
                        let lk = left_lookup_keys
                            .get(&key)
                            .map(|entry| entry.value().clone())
                            .unwrap_or_else(|| lookup_key(&key, left_value.value()));
                        let right = lookup_shadow.get(&lk);
                        mapper(&key, left_value.value(), right.as_ref().map(|r| r.value()))
                    });

                    match (output.get_value(&key), desired) {
                        (Some(old_value), Some(new_value)) => changes.push(MapDiff::Update {
                            key: key.clone(),
                            old_value,
                            new_value,
                        }),
                        (None, Some(new_value)) => changes.push(MapDiff::Insert {
                            key: key.clone(),
                            value: new_value,
                        }),
                        (Some(old_value), None) => changes.push(MapDiff::Remove {
                            key: key.clone(),
                            old_value,
                        }),
                        (None, None) => {}
                    }
                }
                log::trace!(
                    target: "hypha::cell_map::switch_map_lookup",
                    "left recompute emitted_changes={}",
                    changes.len()
                );
                output.apply_batch(changes);
            }
        });

        let output_weak = Arc::downgrade(&output.inner);
        let lookup_guard = lookup.subscribe_diffs({
            let lookup_key = lookup_key.clone();
            let mapper = mapper.clone();
            let left_shadow = left_shadow.clone();
            let left_lookup_keys = left_lookup_keys.clone();
            let lookup_shadow = lookup_shadow.clone();
            move |diff| {
                let Some(inner) = output_weak.upgrade() else {
                    return;
                };
                let output = CellMap::<K, U, CellMutable> {
                    inner,
                    _marker: PhantomData,
                };

                fn apply_lookup<LK, R>(
                    diff: &MapDiff<LK, R>,
                    lookup_shadow: &DashMap<LK, R>,
                    changed_lookup: &mut HashSet<LK>,
                ) where
                    LK: Hash + Eq + CellValue,
                    R: CellValue,
                {
                    match diff {
                        MapDiff::Initial { entries } => {
                            let previous: Vec<LK> = lookup_shadow
                                .iter()
                                .map(|entry| entry.key().clone())
                                .collect();
                            for k in previous {
                                changed_lookup.insert(k);
                            }
                            lookup_shadow.clear();
                            for (k, v) in entries {
                                lookup_shadow.insert(k.clone(), v.clone());
                                changed_lookup.insert(k.clone());
                            }
                        }
                        MapDiff::Insert { key, value }
                        | MapDiff::Update {
                            key,
                            new_value: value,
                            ..
                        } => {
                            lookup_shadow.insert(key.clone(), value.clone());
                            changed_lookup.insert(key.clone());
                        }
                        MapDiff::Remove { key, .. } => {
                            lookup_shadow.remove(key);
                            changed_lookup.insert(key.clone());
                        }
                        MapDiff::Batch { changes } => {
                            for change in changes {
                                apply_lookup(change, lookup_shadow, changed_lookup);
                            }
                        }
                    }
                }

                let mut changed_lookup: HashSet<LK> = HashSet::new();
                apply_lookup(diff, &lookup_shadow, &mut changed_lookup);
                if changed_lookup.is_empty() {
                    log::trace!(
                        target: "hypha::cell_map::switch_map_lookup",
                        "lookup diff processed: changed_lookup=0"
                    );
                    return;
                }

                let impacted: Vec<K> = left_lookup_keys
                    .iter()
                    .filter(|entry| changed_lookup.contains(entry.value()))
                    .map(|entry| entry.key().clone())
                    .collect();
                log::trace!(
                    target: "hypha::cell_map::switch_map_lookup",
                    "lookup diff processed: changed_lookup={} impacted_keys={}",
                    changed_lookup.len(),
                    impacted.len()
                );

                let mut changes: Vec<MapDiff<K, U>> = Vec::new();
                for key in impacted {
                    let desired = left_shadow.get(&key).map(|left_value| {
                        let lk = left_lookup_keys
                            .get(&key)
                            .map(|entry| entry.value().clone())
                            .unwrap_or_else(|| lookup_key(&key, left_value.value()));
                        let right = lookup_shadow.get(&lk);
                        mapper(&key, left_value.value(), right.as_ref().map(|r| r.value()))
                    });

                    match (output.get_value(&key), desired) {
                        (Some(old_value), Some(new_value)) => changes.push(MapDiff::Update {
                            key: key.clone(),
                            old_value,
                            new_value,
                        }),
                        (None, Some(new_value)) => changes.push(MapDiff::Insert {
                            key: key.clone(),
                            value: new_value,
                        }),
                        (Some(old_value), None) => changes.push(MapDiff::Remove {
                            key: key.clone(),
                            old_value,
                        }),
                        (None, None) => {}
                    }
                }
                log::trace!(
                    target: "hypha::cell_map::switch_map_lookup",
                    "lookup recompute emitted_changes={}",
                    changes.len()
                );
                output.apply_batch(changes);
            }
        });

        output.own(left_guard);
        output.own(lookup_guard);
        output.lock()
    }

    #[track_caller]
    fn count_by<GK, F>(&self, group_key: F) -> CellMap<GK, usize, CellImmutable>
    where
        GK: Hash + Eq + CellValue,
        F: Fn(&K, &V) -> GK + Send + Sync + 'static,
    {
        let counts = CellMap::<GK, usize, CellMutable>::new();
        if let Some(parent_name) = (**self.inner.name.load()).as_ref() {
            counts
                .clone()
                .with_name(format!("{}::count_by", parent_name));
        }

        let group_key = Arc::new(group_key);
        let source_to_group = Arc::new(DashMap::<K, GK>::new());
        let group_counts = Arc::new(DashMap::<GK, usize>::new());
        let counts_weak = Arc::downgrade(&counts.inner);

        let guard = self.subscribe_diffs(move |diff| {
            let Some(inner) = counts_weak.upgrade() else {
                return;
            };
            let counts_map = CellMap::<GK, usize, CellMutable> {
                inner,
                _marker: PhantomData,
            };

            fn bump<GK>(
                group: GK,
                delta: isize,
                group_counts: &DashMap<GK, usize>,
                touched: &mut HashSet<GK>,
            ) where
                GK: Hash + Eq + CellValue,
            {
                if delta == 0 {
                    return;
                }
                let current = group_counts.get(&group).map(|v| *v.value()).unwrap_or(0);
                let next = if delta > 0 {
                    current.saturating_add(delta as usize)
                } else {
                    current.saturating_sub((-delta) as usize)
                };
                if next == 0 {
                    group_counts.remove(&group);
                } else {
                    group_counts.insert(group.clone(), next);
                }
                touched.insert(group);
            }

            fn apply_source<K, V, GK>(
                diff: &MapDiff<K, V>,
                group_key: &Arc<dyn Fn(&K, &V) -> GK + Send + Sync>,
                source_to_group: &DashMap<K, GK>,
                group_counts: &DashMap<GK, usize>,
                touched: &mut HashSet<GK>,
            ) where
                K: Hash + Eq + CellValue,
                V: CellValue,
                GK: Hash + Eq + CellValue,
            {
                match diff {
                    MapDiff::Initial { entries } => {
                        let previous_sources: Vec<K> = source_to_group
                            .iter()
                            .map(|entry| entry.key().clone())
                            .collect();
                        for source_key in previous_sources {
                            if let Some((_, old_group)) = source_to_group.remove(&source_key) {
                                bump(old_group, -1, group_counts, touched);
                            }
                        }
                        for (source_key, value) in entries {
                            let group = group_key(source_key, value);
                            source_to_group.insert(source_key.clone(), group.clone());
                            bump(group, 1, group_counts, touched);
                        }
                    }
                    MapDiff::Insert { key, value } => {
                        let group = group_key(key, value);
                        source_to_group.insert(key.clone(), group.clone());
                        bump(group, 1, group_counts, touched);
                    }
                    MapDiff::Update { key, new_value, .. } => {
                        let new_group = group_key(key, new_value);
                        let old_group = source_to_group
                            .insert(key.clone(), new_group.clone())
                            .unwrap_or_else(|| new_group.clone());
                        if old_group != new_group {
                            bump(old_group, -1, group_counts, touched);
                            bump(new_group, 1, group_counts, touched);
                        }
                    }
                    MapDiff::Remove { key, .. } => {
                        if let Some((_, old_group)) = source_to_group.remove(key) {
                            bump(old_group, -1, group_counts, touched);
                        }
                    }
                    MapDiff::Batch { changes } => {
                        for change in changes {
                            apply_source(change, group_key, source_to_group, group_counts, touched);
                        }
                    }
                }
            }

            let group_key_dyn: Arc<dyn Fn(&K, &V) -> GK + Send + Sync> = group_key.clone();
            let mut touched: HashSet<GK> = HashSet::new();
            apply_source(
                diff,
                &group_key_dyn,
                &source_to_group,
                &group_counts,
                &mut touched,
            );
            log::trace!(
                target: "hypha::cell_map::count_by",
                "source diff processed: touched_groups={}",
                touched.len()
            );

            let mut changes: Vec<MapDiff<GK, usize>> = Vec::new();
            for group in touched {
                let before = counts_map.get_value(&group);
                let after = group_counts.get(&group).map(|v| *v.value());
                match (before, after) {
                    (Some(old_value), Some(new_value)) => changes.push(MapDiff::Update {
                        key: group.clone(),
                        old_value,
                        new_value,
                    }),
                    (None, Some(new_value)) => changes.push(MapDiff::Insert {
                        key: group.clone(),
                        value: new_value,
                    }),
                    (Some(old_value), None) => changes.push(MapDiff::Remove {
                        key: group.clone(),
                        old_value,
                    }),
                    (None, None) => {}
                }
            }
            log::trace!(
                target: "hypha::cell_map::count_by",
                "recompute emitted_changes={}",
                changes.len()
            );
            counts_map.apply_batch(changes);
        });

        counts.own(guard);
        counts.lock()
    }

    #[track_caller]
    fn group_by<GK, F>(&self, group_key: F) -> CellMap<GK, Vec<V>, CellImmutable>
    where
        K: Ord,
        GK: Hash + Eq + CellValue,
        F: Fn(&K, &V) -> GK + Send + Sync + 'static,
    {
        let grouped = CellMap::<GK, Vec<V>, CellMutable>::new();
        if let Some(parent_name) = (**self.inner.name.load()).as_ref() {
            grouped
                .clone()
                .with_name(format!("{}::group_by", parent_name));
        }

        let group_key = Arc::new(group_key);
        let source_to_group = Arc::new(DashMap::<K, GK>::new());
        let group_rows = Arc::new(DashMap::<GK, BTreeMap<K, V>>::new());
        let grouped_weak = Arc::downgrade(&grouped.inner);

        let guard = self.subscribe_diffs(move |diff| {
            let Some(inner) = grouped_weak.upgrade() else {
                return;
            };
            let grouped_map = CellMap::<GK, Vec<V>, CellMutable> {
                inner,
                _marker: PhantomData,
            };

            fn apply_source<K, V, GK>(
                diff: &MapDiff<K, V>,
                group_key: &Arc<dyn Fn(&K, &V) -> GK + Send + Sync>,
                source_to_group: &DashMap<K, GK>,
                group_rows: &DashMap<GK, BTreeMap<K, V>>,
                touched: &mut HashSet<GK>,
            ) where
                K: Hash + Eq + Ord + CellValue,
                V: CellValue,
                GK: Hash + Eq + CellValue,
            {
                match diff {
                    MapDiff::Initial { entries } => {
                        let previous_sources: Vec<(K, GK)> = source_to_group
                            .iter()
                            .map(|entry| (entry.key().clone(), entry.value().clone()))
                            .collect();
                        source_to_group.clear();
                        group_rows.clear();
                        for (_, old_group) in previous_sources {
                            touched.insert(old_group);
                        }
                        for (source_key, value) in entries {
                            let group = group_key(source_key, value);
                            source_to_group.insert(source_key.clone(), group.clone());
                            group_rows
                                .entry(group.clone())
                                .or_insert_with(BTreeMap::new)
                                .insert(source_key.clone(), value.clone());
                            touched.insert(group);
                        }
                    }
                    MapDiff::Insert { key, value } => {
                        let group = group_key(key, value);
                        source_to_group.insert(key.clone(), group.clone());
                        group_rows
                            .entry(group.clone())
                            .or_insert_with(BTreeMap::new)
                            .insert(key.clone(), value.clone());
                        touched.insert(group);
                    }
                    MapDiff::Update { key, new_value, .. } => {
                        let new_group = group_key(key, new_value);
                        let old_group = source_to_group
                            .insert(key.clone(), new_group.clone())
                            .unwrap_or_else(|| new_group.clone());
                        if old_group != new_group {
                            if let Some(mut old_rows) = group_rows.get_mut(&old_group) {
                                old_rows.remove(key);
                                if old_rows.is_empty() {
                                    drop(old_rows);
                                    group_rows.remove(&old_group);
                                }
                            }
                            group_rows
                                .entry(new_group.clone())
                                .or_insert_with(BTreeMap::new)
                                .insert(key.clone(), new_value.clone());
                            touched.insert(old_group);
                            touched.insert(new_group);
                        } else {
                            group_rows
                                .entry(new_group.clone())
                                .or_insert_with(BTreeMap::new)
                                .insert(key.clone(), new_value.clone());
                            touched.insert(new_group);
                        }
                    }
                    MapDiff::Remove { key, .. } => {
                        if let Some((_, old_group)) = source_to_group.remove(key) {
                            if let Some(mut old_rows) = group_rows.get_mut(&old_group) {
                                old_rows.remove(key);
                                if old_rows.is_empty() {
                                    drop(old_rows);
                                    group_rows.remove(&old_group);
                                }
                            }
                            touched.insert(old_group);
                        }
                    }
                    MapDiff::Batch { changes } => {
                        for change in changes {
                            apply_source(change, group_key, source_to_group, group_rows, touched);
                        }
                    }
                }
            }

            let group_key_dyn: Arc<dyn Fn(&K, &V) -> GK + Send + Sync> = group_key.clone();
            let mut touched: HashSet<GK> = HashSet::new();
            apply_source(
                diff,
                &group_key_dyn,
                &source_to_group,
                &group_rows,
                &mut touched,
            );
            log::trace!(
                target: "hypha::cell_map::group_by",
                "source diff processed: touched_groups={}",
                touched.len()
            );

            let mut changes: Vec<MapDiff<GK, Vec<V>>> = Vec::new();
            for group in touched {
                let before = grouped_map.get_value(&group);
                let after = group_rows
                    .get(&group)
                    .map(|rows| rows.values().cloned().collect::<Vec<_>>());
                match (before, after) {
                    (Some(old_value), Some(new_value)) => changes.push(MapDiff::Update {
                        key: group.clone(),
                        old_value,
                        new_value,
                    }),
                    (None, Some(new_value)) => changes.push(MapDiff::Insert {
                        key: group.clone(),
                        value: new_value,
                    }),
                    (Some(old_value), None) => changes.push(MapDiff::Remove {
                        key: group.clone(),
                        old_value,
                    }),
                    (None, None) => {}
                }
            }
            log::trace!(
                target: "hypha::cell_map::group_by",
                "recompute emitted_changes={}",
                changes.len()
            );
            grouped_map.apply_batch(changes);
        });

        grouped.own(guard);
        grouped.lock()
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
    use crate::traits::{Gettable, MapExt, Watchable};

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

    #[test]
    fn test_cellmap_select_cell_reacts_to_predicate_changes() {
        let values = CellMap::<String, i32>::new();
        let gates = CellMap::<String, bool>::new();

        values.insert("a".to_string(), 10);
        values.insert("b".to_string(), 20);
        gates.insert("a".to_string(), false);
        gates.insert("b".to_string(), true);

        let filtered = values.select_cell({
            let gates = gates.clone();
            move |key, _value| gates.get(key).map(|v| v.unwrap_or(false))
        });

        assert_eq!(filtered.entries().get().len(), 1);
        assert!(!filtered.contains_key(&"a".to_string()));
        assert!(filtered.contains_key(&"b".to_string()));

        gates.insert("a".to_string(), true);
        assert_eq!(filtered.entries().get().len(), 2);
        assert!(filtered.contains_key(&"a".to_string()));

        gates.insert("b".to_string(), false);
        assert_eq!(filtered.entries().get().len(), 1);
        assert!(!filtered.contains_key(&"b".to_string()));
        assert!(filtered.contains_key(&"a".to_string()));

        values.insert("a".to_string(), 11);
        assert_eq!(filtered.get_value(&"a".to_string()), Some(11));
    }

    #[test]
    fn test_cellmap_switch_map_cell_reacts_per_row() {
        let values = CellMap::<String, i32>::new();
        let factors = CellMap::<String, i32>::new();

        values.insert("a".to_string(), 10);
        values.insert("b".to_string(), 20);
        factors.insert("a".to_string(), 1);
        factors.insert("b".to_string(), 2);

        let out = values.switch_map_cell({
            let factors = factors.clone();
            move |key, value| {
                let v = *value;
                factors.get(key).map(move |f| v * f.unwrap_or(0))
            }
        });

        assert_eq!(out.get_value(&"a".to_string()), Some(10));
        assert_eq!(out.get_value(&"b".to_string()), Some(40));

        factors.insert("a".to_string(), 3);
        assert_eq!(out.get_value(&"a".to_string()), Some(30));
        assert_eq!(out.get_value(&"b".to_string()), Some(40));

        values.insert("b".to_string(), 5);
        assert_eq!(out.get_value(&"b".to_string()), Some(10));

        factors.insert("b".to_string(), 4);
        assert_eq!(out.get_value(&"b".to_string()), Some(20));

        values.remove(&"a".to_string());
        assert_eq!(out.get_value(&"a".to_string()), None);
        factors.insert("a".to_string(), 99);
        assert_eq!(out.get_value(&"a".to_string()), None);
    }

    #[test]
    fn test_cellmap_select_cell_preserves_upstream_batch() {
        let source = CellMap::<String, i32>::new();
        let out = source.select_cell(|_, _| Cell::new(true).lock());

        let seen = Arc::new(Mutex::new(Vec::<MapDiff<String, i32>>::new()));
        let seen_clone = seen.clone();
        let _guard = out.subscribe_diffs(move |diff| {
            seen_clone.lock().unwrap().push(diff.clone());
        });

        source.insert_many(vec![("a".to_string(), 1), ("b".to_string(), 2)]);

        let seen = seen.lock().unwrap();
        assert!(seen.len() >= 2);
        let last = seen.last().unwrap();
        match last {
            MapDiff::Batch { changes } => {
                assert_eq!(changes.len(), 2);
                assert!(matches!(
                    &changes[0],
                    MapDiff::Insert { key, value } if key == "a" && *value == 1
                ));
                assert!(matches!(
                    &changes[1],
                    MapDiff::Insert { key, value } if key == "b" && *value == 2
                ));
            }
            _ => panic!("expected batch diff from select_cell"),
        }
    }

    #[test]
    fn test_cellmap_switch_map_cell_preserves_upstream_batch() {
        let source = CellMap::<String, i32>::new();
        let out = source.switch_map_cell(|_, v| Cell::new(*v * 10).lock());

        let seen = Arc::new(Mutex::new(Vec::<MapDiff<String, i32>>::new()));
        let seen_clone = seen.clone();
        let _guard = out.subscribe_diffs(move |diff| {
            seen_clone.lock().unwrap().push(diff.clone());
        });

        source.insert_many(vec![("a".to_string(), 1), ("b".to_string(), 2)]);

        let seen = seen.lock().unwrap();
        assert!(seen.len() >= 2);
        let last = seen.last().unwrap();
        match last {
            MapDiff::Batch { changes } => {
                assert_eq!(changes.len(), 2);
                assert!(matches!(
                    &changes[0],
                    MapDiff::Insert { key, value } if key == "a" && *value == 10
                ));
                assert!(matches!(
                    &changes[1],
                    MapDiff::Insert { key, value } if key == "b" && *value == 20
                ));
            }
            _ => panic!("expected batch diff from switch_map_cell"),
        }
    }

    #[test]
    fn test_cellmap_select_lookup_preserves_lookup_batch() {
        let left = CellMap::<String, i32>::new();
        left.insert("a".to_string(), 1);
        left.insert("b".to_string(), 2);

        let lookup = CellMap::<String, bool>::new();
        let out = left.select_lookup(
            &lookup,
            |_, v| v.to_string(),
            |_, _, present| present.copied().unwrap_or(false),
        );

        let seen = Arc::new(Mutex::new(Vec::<MapDiff<String, i32>>::new()));
        let seen_clone = seen.clone();
        let _guard = out.subscribe_diffs(move |diff| {
            seen_clone.lock().unwrap().push(diff.clone());
        });

        lookup.insert_many(vec![("1".to_string(), true), ("2".to_string(), true)]);

        let seen = seen.lock().unwrap();
        let last = seen.last().unwrap();
        match last {
            MapDiff::Batch { changes } => assert_eq!(changes.len(), 2),
            _ => panic!("expected batch diff from select_lookup"),
        }
    }

    #[test]
    fn test_cellmap_count_by_preserves_upstream_batch() {
        let source = CellMap::<String, i32>::new();
        let counts = source.count_by(|_, v| v.to_string());

        let seen = Arc::new(Mutex::new(Vec::<MapDiff<String, usize>>::new()));
        let seen_clone = seen.clone();
        let _guard = counts.subscribe_diffs(move |diff| {
            seen_clone.lock().unwrap().push(diff.clone());
        });

        source.insert_many(vec![("a".to_string(), 1), ("b".to_string(), 2)]);

        let seen = seen.lock().unwrap();
        let last = seen.last().unwrap();
        match last {
            MapDiff::Batch { changes } => assert_eq!(changes.len(), 2),
            _ => panic!("expected batch diff from count_by"),
        }
    }

    #[test]
    fn test_cellmap_group_by_preserves_upstream_batch() {
        let source = CellMap::<String, i32>::new();
        let grouped = source.group_by(|_, v| v.to_string());

        let seen = Arc::new(Mutex::new(Vec::<MapDiff<String, Vec<i32>>>::new()));
        let seen_clone = seen.clone();
        let _guard = grouped.subscribe_diffs(move |diff| {
            seen_clone.lock().unwrap().push(diff.clone());
        });

        source.insert_many(vec![("a".to_string(), 1), ("b".to_string(), 2)]);

        let seen = seen.lock().unwrap();
        let last = seen.last().unwrap();
        match last {
            MapDiff::Batch { changes } => assert_eq!(changes.len(), 2),
            _ => panic!("expected batch diff from group_by"),
        }
    }
}
