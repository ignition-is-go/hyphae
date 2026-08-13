//! Reactive `HashMap` with per-key observability.
//!
//! `CellMap` wraps a concurrent `HashMap` where each entry can be individually observed.
//! Changes to keys trigger reactive updates to observers.

use std::{
    collections::HashMap,
    hash::Hash,
    marker::PhantomData,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use dashmap::{DashMap, mapref::entry::Entry};
use parking_lot::Mutex;
use uuid::Uuid;

use crate::{
    cell::{Cell, CellImmutable, CellMutable, WeakCell},
    pipeline::{Definite, Materialize},
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
    Batch { changes: Vec<Self> },
}

pub(crate) struct CellMapInner<K, V>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    /// The actual data storage.
    pub(crate) data: DashMap<K, V>,
    /// Cached per-key observation cells.
    ///
    /// Entries are `WeakCell`s: once every strong handle a caller holds from
    /// `get(k)` drops, the weak dangles but its slot lingers here. Left
    /// unchecked this is an unbounded leak on maps with high key churn (every
    /// distinct key ever observed keeps a slot forever). [`maybe_prune_key_cells`]
    /// amortizes a sweep of dead weaks across mutations to keep it bounded.
    key_cells: DashMap<K, WeakCell<Option<V>, CellMutable>>,
    /// Mutation counter driving amortized pruning of dead `key_cells` weaks.
    prune_ops: AtomicUsize,
    /// Cell for diff notifications.
    pub(crate) diffs_cell: Cell<MapDiff<K, V>, CellMutable>,
    /// Cell for length.
    len_cell: Cell<usize, CellMutable>,
    /// Subscription guards owned by this map (dropped when map drops).
    owned: DashMap<Uuid, SubscriptionGuard>,
    /// Optional name for debugging. Cold path — set once via `with_name`,
    /// read by per-key cell formatting helpers.
    pub(crate) name: Mutex<Option<Arc<str>>>,
}

/// A reactive `HashMap` with per-key observability.
///
/// # Example
///
/// ```
/// use hyphae::{CellMap, Gettable, Materialize, Watchable, Signal};
///
/// let map = CellMap::<String, i32>::new();
///
/// // Observe a specific key
/// let value_cell = map.get(&"counter".to_string()).materialize();
/// assert_eq!(value_cell.get(), None);
///
/// // Insert triggers update
/// map.insert("counter".to_string(), 42);
/// assert_eq!(value_cell.get(), Some(42));
///
/// // Observe all entries
/// let entries = map.entries().materialize();
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

#[derive(Clone)]
struct EntryProjection<K, V>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    entries: Vec<(K, V)>,
    index_by_key: HashMap<K, usize>,
}

impl<K, V> EntryProjection<K, V>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    fn from_entries(entries: Vec<(K, V)>) -> Self {
        let index_by_key = entries
            .iter()
            .enumerate()
            .map(|(idx, (key, _))| (key.clone(), idx))
            .collect();

        Self {
            entries,
            index_by_key,
        }
    }

    fn apply_diff(&mut self, diff: &MapDiff<K, V>) {
        match diff {
            MapDiff::Initial { entries } => {
                *self = Self::from_entries(entries.clone());
            }
            MapDiff::Insert { key, value } => {
                if let Some(entry) = self
                    .index_by_key
                    .get(key)
                    .and_then(|index| self.entries.get_mut(*index))
                {
                    entry.1 = value.clone();
                    return;
                }
                let idx = self.entries.len();
                self.entries.push((key.clone(), value.clone()));
                self.index_by_key.insert(key.clone(), idx);
            }
            MapDiff::Remove { key, .. } => {
                self.remove_key(key);
            }
            MapDiff::Update { key, new_value, .. } => {
                if let Some(entry) = self
                    .index_by_key
                    .get(key)
                    .and_then(|index| self.entries.get_mut(*index))
                {
                    entry.1 = new_value.clone();
                }
            }
            MapDiff::Batch { changes } => {
                for change in changes {
                    self.apply_diff(change);
                }
            }
        }
    }

    fn remove_key(&mut self, key: &K) {
        let Some(idx) = self.index_by_key.remove(key) else {
            return;
        };
        self.entries.swap_remove(idx);
        if idx < self.entries.len()
            && let Some((swapped_key, _)) = self.entries.get(idx)
        {
            self.index_by_key.insert(swapped_key.clone(), idx);
        }
    }
}

#[derive(Clone)]
struct KeyProjection<K>
where
    K: Hash + Eq + CellValue,
{
    keys: Vec<K>,
    index_by_key: HashMap<K, usize>,
}

impl<K> KeyProjection<K>
where
    K: Hash + Eq + CellValue,
{
    fn from_keys(keys: Vec<K>) -> Self {
        let index_by_key = keys
            .iter()
            .enumerate()
            .map(|(idx, key)| (key.clone(), idx))
            .collect();

        Self { keys, index_by_key }
    }

    /// Apply a diff, returning whether the projected key vector changed.
    fn apply_diff<V: CellValue>(&mut self, diff: &MapDiff<K, V>) -> bool {
        match diff {
            MapDiff::Initial { entries } => {
                let keys = entries.iter().map(|(key, _)| key.clone()).collect();
                if self.keys == keys {
                    false
                } else {
                    *self = Self::from_keys(keys);
                    true
                }
            }
            MapDiff::Insert { key, .. } => {
                if self.index_by_key.contains_key(key) {
                    return false;
                }
                let idx = self.keys.len();
                self.keys.push(key.clone());
                self.index_by_key.insert(key.clone(), idx);
                true
            }
            MapDiff::Remove { key, .. } => self.remove_key(key),
            MapDiff::Update { .. } => false,
            MapDiff::Batch { changes } => {
                let mut changed = false;
                for change in changes {
                    changed |= self.apply_diff(change);
                }
                changed
            }
        }
    }

    fn remove_key(&mut self, key: &K) -> bool {
        let Some(idx) = self.index_by_key.remove(key) else {
            return false;
        };
        self.keys.swap_remove(idx);
        if idx < self.keys.len()
            && let Some(swapped_key) = self.keys.get(idx)
        {
            self.index_by_key.insert(swapped_key.clone(), idx);
        }
        true
    }
}

#[derive(Clone)]
struct ValueProjection<K, V>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    items: Vec<V>,
    index_by_key: HashMap<K, usize>,
    keys_by_index: Vec<K>,
}

impl<K, V> ValueProjection<K, V>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    fn from_entries(entries: Vec<(K, V)>) -> Self {
        let mut items = Vec::with_capacity(entries.len());
        let mut keys_by_index = Vec::with_capacity(entries.len());
        let mut index_by_key = HashMap::with_capacity(entries.len());

        for (idx, (key, value)) in entries.into_iter().enumerate() {
            index_by_key.insert(key.clone(), idx);
            keys_by_index.push(key);
            items.push(value);
        }

        Self {
            items,
            index_by_key,
            keys_by_index,
        }
    }

    fn apply_diff(&mut self, diff: &MapDiff<K, V>) {
        match diff {
            MapDiff::Initial { entries } => {
                *self = Self::from_entries(entries.clone());
            }
            MapDiff::Insert { key, value } => {
                if let Some(item) = self
                    .index_by_key
                    .get(key)
                    .and_then(|index| self.items.get_mut(*index))
                {
                    *item = value.clone();
                    return;
                }
                let idx = self.items.len();
                self.index_by_key.insert(key.clone(), idx);
                self.keys_by_index.push(key.clone());
                self.items.push(value.clone());
            }
            MapDiff::Remove { key, .. } => {
                self.remove_key(key);
            }
            MapDiff::Update { key, new_value, .. } => {
                if let Some(item) = self
                    .index_by_key
                    .get(key)
                    .and_then(|index| self.items.get_mut(*index))
                {
                    *item = new_value.clone();
                }
            }
            MapDiff::Batch { changes } => {
                for change in changes {
                    self.apply_diff(change);
                }
            }
        }
    }

    fn remove_key(&mut self, key: &K) {
        let Some(idx) = self.index_by_key.remove(key) else {
            return;
        };
        self.items.swap_remove(idx);
        self.keys_by_index.swap_remove(idx);
        if idx < self.keys_by_index.len()
            && let Some(swapped_key) = self.keys_by_index.get(idx)
        {
            self.index_by_key.insert(swapped_key.clone(), idx);
        }
    }
}

impl<K, V> CellMap<K, V, CellMutable>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    /// Create a new empty `CellMap`.
    #[track_caller]
    #[must_use]
    pub fn new() -> Self {
        let diffs_cell = Cell::new(MapDiff::Initial {
            entries: Vec::new(),
        });
        // A diffs stream is events, not a level: each MapDiff is a distinct
        // add/remove/change an accumulating subscriber must see in order. Under
        // `batch`, coalescing would drop intermediate diffs (an add+remove of the
        // same key in one batch collapses to just the remove), silently — and
        // invisibly to fanout-count metrics. Exempt it from coalescing by default
        // so no `batch` caller can trip that; the cost is at most an extra
        // fanout pass for snapshot-rebuild subscribers under `batch`, never a
        // wrong result.
        #[cfg(feature = "scheduler")]
        let diffs_cell = diffs_cell.no_coalesce();
        let len_cell = Cell::new(0);

        // Mark len_cell as owned by diffs_cell so it doesn't appear as an orphan root

        Self {
            inner: Arc::new(CellMapInner {
                data: DashMap::new(),
                key_cells: DashMap::new(),
                prune_ops: AtomicUsize::new(0),
                diffs_cell,
                len_cell,
                owned: DashMap::new(),
                name: Mutex::new(None),
            }),
            _marker: PhantomData,
        }
    }

    /// Own a subscription guard, keeping it alive as long as this `CellMap` exists.
    pub fn own(&self, guard: SubscriptionGuard) {
        self.inner.owned.insert(Uuid::new_v4(), guard);
    }

    /// Own a subscription guard, keeping it alive as long as this `CellMap` exists.
    ///
    /// This enables building custom reactive `CellMaps` driven by external cells.
    pub fn own_guard(&self, guard: SubscriptionGuard) {
        self.own(guard);
    }

    /// Amortized sweep of dead `key_cells` weaks.
    ///
    /// Every distinct key ever passed to [`get`](Self::get) leaves a
    /// `WeakCell` slot behind; once the observing `Cell` drops, that slot
    /// dangles. On a map with high key churn those dead slots accumulate
    /// without bound (the production OOM this guards against). This runs
    /// `key_cells.retain(|_, w| w.is_alive())` roughly once every
    /// `key_cells.len()` mutations, so the sweep cost amortizes to O(1) per
    /// mutation while bounding `key_cells` to the live observed-key set.
    ///
    /// Pruning only ever drops weaks whose `Cell` has no strong handles, so it
    /// cannot change observable behavior: a dead weak would upgrade to `None`
    /// and notify nobody anyway. Live weaks — including a re-observed key whose
    /// cell is still held — are always retained, preserving reinsert re-notify.
    ///
    /// MUST be called before this method takes any `key_cells` shard `Ref`:
    /// `retain` locks every shard, so holding a `Ref` across it would deadlock.
    fn maybe_prune_key_cells(&self) {
        let key_cells = &self.inner.key_cells;
        let len = key_cells.len();
        if len == 0 {
            return;
        }
        // Sweep about once per `len` mutations, with a floor so tiny maps don't
        // sweep on nearly every op.
        let threshold = len.max(32);
        if self
            .inner
            .prune_ops
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1)
            < threshold
        {
            return;
        }
        self.inner.prune_ops.store(0, Ordering::Relaxed);
        key_cells.retain(|_, weak| weak.is_alive());
    }

    fn notify_len_if_changed(&self, previous_len: usize) {
        let len = self.inner.data.len();
        if len != previous_len {
            self.inner.len_cell.set(len);
        }
    }

    /// Insert a key-value pair, returning the old value if present.
    pub fn insert(&self, key: K, value: V) -> Option<V> {
        self.maybe_prune_key_cells();
        let old = self.inner.data.insert(key.clone(), value.clone());

        // No-op update: same key/value should not emit a diff or notify observers.
        if old.as_ref().is_some_and(|old_value| old_value == &value) {
            return old;
        }

        // Notify per-key observers (O(1))
        if let Some(weak) = self.inner.key_cells.get(&key)
            && let Some(cell) = weak.upgrade()
        {
            cell.set(Some(value.clone()));
        }

        let diff = old
            .as_ref()
            .map(|old_value| MapDiff::Update {
                key: key.clone(),
                old_value: old_value.clone(),
                new_value: value.clone(),
            })
            .unwrap_or(MapDiff::Insert { key, value });
        self.inner.diffs_cell.set(diff);
        if old.is_none() {
            self.inner.len_cell.set(self.inner.data.len());
        }

        old
    }

    /// Insert multiple key-value pairs and emit a single batch diff.
    pub fn insert_many(&self, entries: Vec<(K, V)>) {
        if entries.is_empty() {
            return;
        }
        self.maybe_prune_key_cells();
        let previous_len = self.inner.data.len();

        let mut changes = Vec::with_capacity(entries.len());
        for (key, value) in entries {
            let old = self.inner.data.insert(key.clone(), value.clone());
            if old.as_ref().is_some_and(|old_value| old_value == &value) {
                continue;
            }
            let diff = old.map_or_else(
                || MapDiff::Insert {
                    key: key.clone(),
                    value: value.clone(),
                },
                |old_value| MapDiff::Update {
                    key: key.clone(),
                    old_value,
                    new_value: value.clone(),
                },
            );
            changes.push(diff);

            if let Some(weak) = self.inner.key_cells.get(&key)
                && let Some(cell) = weak.upgrade()
            {
                cell.set(Some(value));
            }
        }

        if changes.is_empty() {
            return;
        }

        self.inner.diffs_cell.set(MapDiff::Batch { changes });
        self.notify_len_if_changed(previous_len);
    }

    /// Remove a key, returning the old value if present.
    pub fn remove(&self, key: &K) -> Option<V> {
        self.maybe_prune_key_cells();
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
        self.maybe_prune_key_cells();

        let original_len = self.inner.data.len();
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

        if self.inner.data.is_empty() && changes.len() == original_len {
            self.inner.diffs_cell.set(MapDiff::Initial {
                entries: Vec::new(),
            });
        } else {
            self.inner.diffs_cell.set(MapDiff::Batch { changes });
        }
        self.inner.len_cell.set(self.inner.data.len());
    }

    /// Replace all entries atomically, emitting a single `Batch` diff.
    ///
    /// Removes keys not in `entries`, inserts/updates keys that are.
    /// Skips no-op updates (existing key with equal value).
    /// Emits `MapDiff::Batch` with the actual changes so downstream
    /// subscribers see one atomic replacement instead of N individual diffs.
    pub fn replace_all(&self, entries: Vec<(K, V)>) {
        self.maybe_prune_key_cells();
        let previous_len = self.inner.data.len();
        let new_keys: std::collections::HashSet<&K> = entries.iter().map(|(k, _)| k).collect();
        let mut changes = Vec::new();

        // Remove keys not in new set
        let keys_to_remove: Vec<K> = self
            .inner
            .data
            .iter()
            .filter(|r| !new_keys.contains(r.key()))
            .map(|r| r.key().clone())
            .collect();

        for key in &keys_to_remove {
            if let Some((k, old_value)) = self.inner.data.remove(key) {
                changes.push(MapDiff::Remove {
                    key: k.clone(),
                    old_value,
                });
                if let Some(weak) = self.inner.key_cells.get(&k)
                    && let Some(cell) = weak.upgrade()
                {
                    cell.set(None);
                }
            }
        }

        // Insert/update entries
        for (key, value) in entries {
            let old = self.inner.data.insert(key.clone(), value.clone());
            if old.as_ref().is_some_and(|old_value| old_value == &value) {
                continue;
            }
            changes.push(old.map_or_else(
                || MapDiff::Insert {
                    key: key.clone(),
                    value: value.clone(),
                },
                |old_value| MapDiff::Update {
                    key: key.clone(),
                    old_value,
                    new_value: value.clone(),
                },
            ));
            if let Some(weak) = self.inner.key_cells.get(&key)
                && let Some(cell) = weak.upgrade()
            {
                cell.set(Some(value));
            }
        }

        if !changes.is_empty() {
            self.inner.diffs_cell.set(MapDiff::Batch { changes });
        }
        self.notify_len_if_changed(previous_len);
    }

    /// Apply a single owned diff to this map and emit it directly (no Batch wrap).
    ///
    /// The single-diff counterpart to [`apply_batch`](Self::apply_batch):
    /// routing one diff through `apply_batch(vec![diff])` allocates a
    /// 1-element `Vec` and wraps the emission in `MapDiff::Batch` for
    /// nothing. Used by the `MapQuery` materialize sink and by downstream
    /// per-diff routers (e.g. myko's `belongs_to` bucket index). Caller must
    /// own the diff (one upstream clone is unavoidable when the source
    /// hands out `&diff`).
    pub fn apply_diff_owned(&self, diff: MapDiff<K, V>) {
        if let MapDiff::Batch { changes } = diff {
            self.apply_batch(changes);
            return;
        }
        self.maybe_prune_key_cells();
        let previous_len = self.inner.data.len();
        match &diff {
            MapDiff::Initial { entries } => {
                let stale_keys: Vec<K> = self.inner.data.iter().map(|r| r.key().clone()).collect();
                for key in stale_keys {
                    self.inner.data.remove(&key);
                    if let Some(weak) = self.inner.key_cells.get(&key)
                        && let Some(cell) = weak.upgrade()
                    {
                        cell.set(None);
                    }
                }
                for (key, value) in entries {
                    self.inner.data.insert(key.clone(), value.clone());
                    if let Some(weak) = self.inner.key_cells.get(key)
                        && let Some(cell) = weak.upgrade()
                    {
                        cell.set(Some(value.clone()));
                    }
                }
            }
            MapDiff::Insert { key, value } => {
                self.inner.data.insert(key.clone(), value.clone());
                if let Some(weak) = self.inner.key_cells.get(key)
                    && let Some(cell) = weak.upgrade()
                {
                    cell.set(Some(value.clone()));
                }
            }
            MapDiff::Remove { key, .. } => {
                self.inner.data.remove(key);
                if let Some(weak) = self.inner.key_cells.get(key)
                    && let Some(cell) = weak.upgrade()
                {
                    cell.set(None);
                }
            }
            MapDiff::Update { key, new_value, .. } => {
                if self
                    .inner
                    .data
                    .get(key)
                    .is_some_and(|existing| existing.value() == new_value)
                {
                    return;
                }
                self.inner.data.insert(key.clone(), new_value.clone());
                if let Some(weak) = self.inner.key_cells.get(key)
                    && let Some(cell) = weak.upgrade()
                {
                    cell.set(Some(new_value.clone()));
                }
            }
            MapDiff::Batch { changes } => {
                self.apply_batch(changes.clone());
                return;
            }
        }

        self.notify_len_if_changed(previous_len);
        self.inner.diffs_cell.set(diff);
    }

    /// Apply a batch of diffs and emit them as one `MapDiff::Batch`.
    pub fn apply_batch(&self, changes: Vec<MapDiff<K, V>>) {
        fn apply_one<K, V>(
            map: &CellMap<K, V, CellMutable>,
            diff: MapDiff<K, V>,
        ) -> Option<MapDiff<K, V>>
        where
            K: Hash + Eq + CellValue,
            V: CellValue,
        {
            if let MapDiff::Batch { changes } = diff {
                let applied: Vec<_> = changes
                    .into_iter()
                    .filter_map(|change| apply_one(map, change))
                    .collect();
                return (!applied.is_empty()).then_some(MapDiff::Batch { changes: applied });
            }
            match &diff {
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
                    if map
                        .inner
                        .data
                        .get(key)
                        .is_some_and(|existing| existing.value() == new_value)
                    {
                        return None;
                    }
                    map.inner.data.insert(key.clone(), new_value.clone());
                    if let Some(weak) = map.inner.key_cells.get(key)
                        && let Some(cell) = weak.upgrade()
                    {
                        cell.set(Some(new_value.clone()));
                    }
                }
                MapDiff::Batch { .. } => return None,
            }
            Some(diff)
        }

        if changes.is_empty() {
            return;
        }
        self.maybe_prune_key_cells();
        let previous_len = self.inner.data.len();
        let applied_changes: Vec<_> = changes
            .into_iter()
            .filter_map(|change| apply_one(self, change))
            .collect();
        if applied_changes.is_empty() {
            return;
        }
        self.notify_len_if_changed(previous_len);
        self.inner.diffs_cell.set(MapDiff::Batch {
            changes: applied_changes,
        });
    }

    /// Give this `CellMap` a name for debugging. Names its internal cells accordingly.
    #[must_use]
    pub fn with_name(self, name: impl Into<Arc<str>>) -> Self {
        let name: Arc<str> = name.into();
        drop(
            self.inner
                .diffs_cell
                .clone()
                .with_name(format!("{name}::diffs")),
        );
        drop(
            self.inner
                .len_cell
                .clone()
                .with_name(format!("{name}::len")),
        );
        *self.inner.name.lock() = Some(name);
        self
    }

    /// Lock the map to prevent further mutations.
    #[must_use]
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
    #[must_use]
    pub fn downgrade(&self) -> WeakCellMap<K, V> {
        WeakCellMap {
            inner: Arc::downgrade(&self.inner),
        }
    }

    /// Build an observable pipeline for a specific key.
    ///
    /// Materializing produces a `Cell<Option<V>>` that updates whenever the
    /// key's value changes. Multiple installations currently reuse the same
    /// underlying per-key cell, but that cache is not part of the public API.
    #[track_caller]
    pub fn get(&self, key: &K) -> impl Materialize<Option<V>, Definite> + use<K, V, M> {
        // Snapshot before entering the key-cell shard: mutation paths update data
        // before observing this cache, so holding both locks would invert their order.
        let initial = self.inner.data.get(key).map(|entry| entry.value().clone());
        let candidate = Cell::new(initial.clone());
        if let Some(map_name) = self.inner.name.lock().as_ref() {
            drop(candidate.clone().with_name(format!("{map_name}[{key:?}]")));
        }

        // Install exactly one live cell per key. Concurrent `get` calls must not
        // return different cells, because only the cached winner receives diffs.
        let cell = match self.inner.key_cells.entry(key.clone()) {
            Entry::Occupied(mut slot) => {
                if let Some(existing) = slot.get().upgrade() {
                    return existing.lock();
                }
                slot.insert(candidate.downgrade());
                candidate
            }
            Entry::Vacant(slot) => {
                slot.insert(candidate.downgrade());
                candidate
            }
        };

        // Close the data-read/cache-install race. Do not overwrite a concurrent
        // mutation that already updated the newly cached cell.
        let latest = self.inner.data.get(key).map(|entry| entry.value().clone());
        if latest != initial && cell.get() == initial {
            cell.set(latest);
        }

        cell.lock()
    }

    /// Build an observable pipeline of all entries.
    ///
    /// Returns a derived cell that maintains entries incrementally via diffs.
    /// The initial value is computed from the current map state, then updates
    /// are applied incrementally as O(1) operations per diff.
    #[track_caller]
    #[must_use]
    pub fn entries(&self) -> impl Materialize<Vec<(K, V)>, Definite> + use<K, V, M> {
        let initial: Vec<(K, V)> = self
            .inner
            .data
            .iter()
            .map(|r| (r.key().clone(), r.value().clone()))
            .collect();
        let state = Arc::new(std::sync::Mutex::new(EntryProjection::from_entries(
            initial.clone(),
        )));

        let cell = Cell::new(initial);
        if let Some(map_name) = self.inner.name.lock().as_ref() {
            drop(cell.clone().with_name(format!("{map_name}::entries")));
        }
        let weak_cell = cell.downgrade();

        // Keep CellMapInner alive as long as this subscription exists.
        // When select() uses a weak ref in its closure, the CellMapInner would otherwise
        // be dropped once the temporary CellMap from select() goes out of scope.
        // This keepalive ensures the filtered CellMap (and its source subscription) survive
        // as long as the entries Cell is alive.
        let map_keepalive = self.inner.clone();

        let first = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let guard = self.inner.diffs_cell.subscribe(move |signal| {
            let _ = &map_keepalive; // prevent drop until closure is dropped
            if first.swap(false, std::sync::atomic::Ordering::SeqCst) {
                return;
            }
            let Some(cell) = weak_cell.upgrade() else {
                return; // Entries cell was dropped
            };
            if let Signal::Value(diff) = signal {
                let Ok(mut projection) = state.lock() else {
                    return;
                };
                projection.apply_diff(diff.as_ref());
                cell.set(projection.entries.clone());
            }
        });

        // Own the subscription guard — this also marks diffs_cell as owned by entries cell
        cell.own(guard);

        cell.lock()
    }

    /// Build an observable pipeline of all values.
    ///
    /// This maintains its own diff-driven projection to avoid forcing an
    /// intermediate entries materialization on hot value-only paths.
    #[track_caller]
    #[must_use]
    pub fn items(&self) -> impl Materialize<Vec<V>, Definite> + use<K, V, M> {
        let initial: Vec<(K, V)> = self
            .inner
            .data
            .iter()
            .map(|r| (r.key().clone(), r.value().clone()))
            .collect();
        let state = Arc::new(std::sync::Mutex::new(ValueProjection::from_entries(
            initial.clone(),
        )));

        let cell = Cell::new(initial.into_iter().map(|(_, value)| value).collect());
        if let Some(map_name) = self.inner.name.lock().as_ref() {
            drop(cell.clone().with_name(format!("{map_name}::items")));
        }
        let weak_cell = cell.downgrade();
        let map_keepalive = self.inner.clone();

        let first = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let guard = self.inner.diffs_cell.subscribe(move |signal| {
            let _ = &map_keepalive;
            if first.swap(false, std::sync::atomic::Ordering::SeqCst) {
                return;
            }
            let Some(cell) = weak_cell.upgrade() else {
                return;
            };
            if let Signal::Value(diff) = signal {
                let Ok(mut projection) = state.lock() else {
                    return;
                };
                projection.apply_diff(diff.as_ref());
                cell.set(projection.items.clone());
            }
        });

        cell.own(guard);
        cell.lock()
    }

    /// Build an observable pipeline of all keys.
    #[track_caller]
    #[must_use]
    pub fn keys(&self) -> impl Materialize<Vec<K>, Definite> + use<K, V, M> {
        let initial: Vec<K> = self
            .inner
            .data
            .iter()
            .map(|entry| entry.key().clone())
            .collect();
        let state = Arc::new(std::sync::Mutex::new(KeyProjection::from_keys(
            initial.clone(),
        )));
        let cell = Cell::new(initial);
        if let Some(map_name) = self.inner.name.lock().as_ref() {
            drop(cell.clone().with_name(format!("{map_name}::keys")));
        }
        let weak_cell = cell.downgrade();
        let map_keepalive = self.inner.clone();
        let first = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let guard = self.inner.diffs_cell.subscribe(move |signal| {
            let _ = &map_keepalive;
            if first.swap(false, Ordering::SeqCst) {
                return;
            }
            let Some(cell) = weak_cell.upgrade() else {
                return;
            };
            if let Signal::Value(diff) = signal {
                let Ok(mut projection) = state.lock() else {
                    return;
                };
                if projection.apply_diff(diff.as_ref()) {
                    cell.set(projection.keys.clone());
                }
            }
        });
        cell.own(guard);
        cell.lock()
    }

    /// Get an observable Cell of the map size.
    ///
    /// This is the preferred reactive count operator because it reuses the
    /// internally maintained length cell instead of materializing entries.
    #[must_use]
    pub fn size(&self) -> impl Materialize<usize, Definite> + use<K, V, M> {
        // A derived size Cell that RETAINS its parent map, mirroring
        // entries()/items()/subscribe_diffs. Returning a bare
        // `len_cell.clone().lock()` captured no keepalive, so a `.size()` cloned
        // out of a temporary CellMap (e.g. `query.materialize().size()`, or
        // myko's `query_map_by_str(q).size()`) let the CellMapInner — and the
        // source subscription that keeps len_cell updating — drop at the end of
        // the statement, silently freezing the count. A fresh cell subscribing
        // to len_cell (rather than owning a keepalive on len_cell itself, which
        // would form a self-cycle through CellMapInner and leak) holds the map
        // alive exactly as long as the returned size Cell is held.
        let cell = Cell::new(self.inner.data.len());
        let weak_cell = cell.downgrade();
        let map_keepalive = self.inner.clone();
        let guard = self.inner.len_cell.subscribe(move |signal| {
            let _ = &map_keepalive; // hold the parent map alive while size Cell lives
            let Some(cell) = weak_cell.upgrade() else {
                return; // size Cell was dropped
            };
            if let Signal::Value(n) = signal {
                cell.set(**n);
            }
        });
        cell.own(guard);
        cell.lock()
    }

    /// Get an observable Cell of the map length.
    ///
    /// Alias for [`CellMap::size`].
    #[must_use]
    pub fn len(&self) -> impl Materialize<usize, Definite> + use<K, V, M> {
        self.size()
    }

    /// Check if map is empty (non-reactive).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inner.data.is_empty()
    }

    /// Get an observable Cell of diff notifications.
    ///
    /// Emits `MapDiff` updates. Starts with `MapDiff::Initial { entries: vec![] }`.
    #[must_use]
    pub fn diffs(&self) -> impl Materialize<MapDiff<K, V>, Definite> + use<K, V, M> {
        self.inner.diffs_cell.clone().lock()
    }

    /// Visit every entry by reference, without allocating (non-reactive).
    ///
    /// The borrowing counterpart to [`snapshot`](Self::snapshot), for
    /// one-shot reads that only need to LOOK at entries — `snapshot()`
    /// allocates a `Vec` and clones every key/value even when the caller
    /// immediately discards it.
    ///
    /// IMPORTANT: iteration holds the underlying shard locks — `f` must
    /// not call any mutating method on THIS map (insert/remove/apply_*),
    /// or it may deadlock. Mutate after `for_each` returns, or use
    /// `snapshot()` when the loop body must write back.
    pub fn for_each(&self, mut f: impl FnMut(&K, &V)) {
        for entry in &self.inner.data {
            f(entry.key(), entry.value());
        }
    }

    /// Get a point-in-time snapshot of all entries (non-reactive).
    ///
    /// Unlike `entries()`, this does NOT create a Cell or subscribe to changes.
    /// Use this for one-shot reads where you don't need live updates.
    #[must_use]
    pub fn snapshot(&self) -> Vec<(K, V)> {
        self.inner
            .data
            .iter()
            .map(|r| (r.key().clone(), r.value().clone()))
            .collect()
    }

    /// Return the current values without creating a reactive projection.
    #[must_use]
    pub fn items_snapshot(&self) -> Vec<V> {
        self.inner
            .data
            .iter()
            .map(|entry| entry.value().clone())
            .collect()
    }

    /// Return the current keys without creating a reactive projection.
    #[must_use]
    pub fn keys_snapshot(&self) -> Vec<K> {
        self.inner
            .data
            .iter()
            .map(|entry| entry.key().clone())
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
        let diffs = self.diffs().materialize();
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
    #[must_use]
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
mod tests {
    use std::sync::{
        Arc, Barrier,
        atomic::{AtomicUsize, Ordering},
    };

    use super::*;
    use crate::traits::{Gettable, Watchable};

    const KEY_CELL_CHURN: u64 = 2_000;

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
        let cell_a = map.get(&"a".to_string()).materialize();
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
        let entries = map.entries().materialize();

        assert_eq!(entries.get(), vec![]);

        map.insert("a".to_string(), 1);
        assert_eq!(entries.get().len(), 1);

        map.insert("b".to_string(), 2);
        assert_eq!(entries.get().len(), 2);

        map.remove(&"a".to_string());
        assert_eq!(entries.get().len(), 1);
    }

    #[test]
    fn test_cellmap_size_observation() {
        let map = CellMap::<String, i32>::new();
        let size = map.size().materialize();

        assert_eq!(size.get(), 0);

        map.insert("a".to_string(), 1);
        assert_eq!(size.get(), 1);

        map.insert("b".to_string(), 2);
        assert_eq!(size.get(), 2);

        map.insert("b".to_string(), 2);
        assert_eq!(size.get(), 2);

        map.remove(&"a".to_string());
        assert_eq!(size.get(), 1);
    }

    #[test]
    fn test_cellmap_items_observation() {
        let map = CellMap::<String, i32>::new();
        let items = map.items().materialize();

        assert_eq!(items.get(), Vec::<i32>::new());

        map.insert("a".to_string(), 1);
        assert_eq!(items.get(), vec![1]);

        map.insert("b".to_string(), 2);
        assert_eq!(items.get(), vec![1, 2]);

        map.insert("a".to_string(), 3);
        assert_eq!(items.get(), vec![3, 2]);

        map.remove(&"b".to_string());
        assert_eq!(items.get(), vec![3]);
    }

    #[test]
    fn test_cellmap_diffs() {
        let map = CellMap::<String, i32>::new();
        let diffs = map.diffs().materialize();

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

        let (tx, rx) = std::sync::mpsc::channel::<MapDiff<String, i32>>();
        let _guard = map.subscribe_diffs(move |diff| {
            let _ = tx.send(diff.clone());
        });

        // Should have received Initial with both entries
        let diffs: Vec<_> = rx.try_iter().collect();
        assert_eq!(diffs.len(), 1);
        assert!(matches!(
            diffs.first(),
            Some(MapDiff::Initial { entries }) if entries.len() == 2
        ));

        // Insert should trigger diff
        map.insert("c".to_string(), 3);
        let diffs: Vec<_> = rx.try_iter().collect();
        assert_eq!(diffs.len(), 1);
        assert!(
            matches!(diffs.first(), Some(MapDiff::Insert { key, value }) if key == "c" && *value == 3)
        );
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
        assert!(matches!(seen.first(), Some(MapDiff::Initial { entries }) if entries.is_empty()));
        assert!(matches!(seen.get(1), Some(MapDiff::Batch { changes }) if changes.len() == 2));
    }

    #[test]
    fn test_insert_same_value_is_noop_update() {
        let map = CellMap::<String, i32>::new();
        let (tx, rx) = std::sync::mpsc::channel::<MapDiff<String, i32>>();

        let _guard = map.subscribe_diffs(move |diff| {
            let _ = tx.send(diff.clone());
        });

        map.insert("a".to_string(), 1);
        map.insert("a".to_string(), 1);

        let seen: Vec<_> = rx.try_iter().collect();
        assert_eq!(seen.len(), 2);
        assert!(matches!(seen.first(), Some(MapDiff::Initial { entries }) if entries.is_empty()));
        assert!(
            matches!(seen.get(1), Some(MapDiff::Insert { key, value }) if key == "a" && *value == 1)
        );
    }

    #[test]
    fn test_apply_batch_filters_noop_updates() {
        let map = CellMap::<String, i32>::new();
        let (tx, rx) = std::sync::mpsc::channel::<MapDiff<String, i32>>();

        map.insert("a".to_string(), 1);
        let _guard = map.subscribe_diffs(move |diff| {
            let _ = tx.send(diff.clone());
        });

        map.apply_batch(vec![
            MapDiff::Update {
                key: "a".to_string(),
                old_value: 1,
                new_value: 1,
            },
            MapDiff::Update {
                key: "a".to_string(),
                old_value: 1,
                new_value: 2,
            },
        ]);

        let seen: Vec<_> = rx.try_iter().collect();
        assert_eq!(seen.len(), 2);
        assert!(matches!(seen.first(), Some(MapDiff::Initial { entries }) if entries.len() == 1));
        assert!(matches!(
            seen.get(1),
            Some(MapDiff::Batch { changes })
                if matches!(changes.as_slice(), [MapDiff::Update { key, old_value: 1, new_value: 2 }] if key == "a")
        ));

        assert_eq!(map.get_value(&"a".to_string()), Some(2));
    }

    #[test]
    fn test_remove_many_full_clear_emits_empty_initial() {
        let map = CellMap::<String, i32>::new();
        let (tx, rx) = std::sync::mpsc::channel::<MapDiff<String, i32>>();

        map.insert("a".to_string(), 1);
        map.insert("b".to_string(), 2);

        let _guard = map.subscribe_diffs(move |diff| {
            let _ = tx.send(diff.clone());
        });

        map.remove_many(vec!["a".to_string(), "b".to_string()]);

        let seen: Vec<_> = rx.try_iter().collect();
        assert_eq!(seen.len(), 2);
        assert!(matches!(seen.first(), Some(MapDiff::Initial { entries }) if entries.len() == 2));
        assert!(matches!(seen.get(1), Some(MapDiff::Initial { entries }) if entries.is_empty()));
    }

    #[test]
    fn test_cellmap_len() {
        let map = CellMap::<String, i32>::new();
        let len = map.len().materialize();

        assert_eq!(len.get(), 0);

        map.insert("a".to_string(), 1);
        assert_eq!(len.get(), 1);

        map.insert("b".to_string(), 2);
        assert_eq!(len.get(), 2);

        map.remove(&"a".to_string());
        assert_eq!(len.get(), 1);
    }

    #[test]
    fn test_len_notifies_only_for_cardinality_changes_across_mutation_paths() {
        let map = CellMap::<String, i32>::new();
        let len = map.len().materialize();
        let notifications = Arc::new(AtomicUsize::new(0));
        let observed = notifications.clone();
        let _guard = len.subscribe(move |_| {
            observed.fetch_add(1, Ordering::SeqCst);
        });
        assert_eq!(notifications.load(Ordering::SeqCst), 1);

        map.insert("a".into(), 1);
        assert_eq!(notifications.load(Ordering::SeqCst), 2);
        map.insert("a".into(), 2);
        assert_eq!(notifications.load(Ordering::SeqCst), 2);

        map.insert_many(vec![("a".into(), 3), ("b".into(), 4)]);
        assert_eq!(notifications.load(Ordering::SeqCst), 3);
        map.insert_many(vec![("a".into(), 5), ("b".into(), 6)]);
        assert_eq!(notifications.load(Ordering::SeqCst), 3);

        // Replacing keys and values at the same cardinality must not wake size.
        map.replace_all(vec![("a".into(), 7), ("c".into(), 8)]);
        assert_eq!(notifications.load(Ordering::SeqCst), 3);

        map.apply_diff_owned(MapDiff::Update {
            key: "a".into(),
            old_value: 7,
            new_value: 9,
        });
        assert_eq!(notifications.load(Ordering::SeqCst), 3);
        map.apply_batch(vec![
            MapDiff::Remove {
                key: "c".into(),
                old_value: 8,
            },
            MapDiff::Insert {
                key: "d".into(),
                value: 10,
            },
        ]);
        assert_eq!(notifications.load(Ordering::SeqCst), 3);
        map.apply_diff_owned(MapDiff::Initial {
            entries: vec![("x".into(), 1), ("y".into(), 2)],
        });
        assert_eq!(notifications.load(Ordering::SeqCst), 3);

        map.remove_many(vec!["x".into()]);
        assert_eq!(len.get(), 1);
        assert_eq!(notifications.load(Ordering::SeqCst), 4);
        map.replace_all(vec![]);
        assert_eq!(notifications.load(Ordering::SeqCst), 5);
    }

    #[test]
    fn test_keys_projection_ignores_value_updates() {
        let map = CellMap::<String, i32>::new();
        map.insert("a".into(), 1);
        let keys = map.keys().materialize();
        let notifications = Arc::new(AtomicUsize::new(0));
        let observed = notifications.clone();
        let _guard = keys.subscribe(move |_| {
            observed.fetch_add(1, Ordering::SeqCst);
        });
        assert_eq!(notifications.load(Ordering::SeqCst), 1);

        map.insert("a".into(), 2);
        map.apply_diff_owned(MapDiff::Update {
            key: "a".into(),
            old_value: 2,
            new_value: 3,
        });
        map.apply_batch(vec![MapDiff::Update {
            key: "a".into(),
            old_value: 3,
            new_value: 4,
        }]);
        assert_eq!(notifications.load(Ordering::SeqCst), 1);
        assert_eq!(keys.get(), vec!["a".to_string()]);

        map.insert("b".into(), 5);
        assert_eq!(notifications.load(Ordering::SeqCst), 2);
        let mut projected = keys.get();
        projected.sort();
        assert_eq!(projected, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn test_key_and_item_snapshots_are_one_shot_views() {
        let map = CellMap::<String, i32>::new();
        map.insert_many(vec![("a".into(), 1), ("b".into(), 2)]);

        let mut keys = map.keys_snapshot();
        keys.sort();
        let mut items = map.items_snapshot();
        items.sort_unstable();
        assert_eq!(keys, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(items, vec![1, 2]);

        map.insert("c".into(), 3);
        assert_eq!(keys.len(), 2);
        assert_eq!(items.len(), 2);
    }

    #[test]
    fn test_cellmap_lock() {
        let map = CellMap::<String, i32>::new();
        map.insert("a".to_string(), 1);

        let locked = map.lock();

        // Can still observe
        assert_eq!(locked.get(&"a".to_string()).materialize().get(), Some(1));
        assert_eq!(locked.entries().materialize().get().len(), 1);

        // But can't mutate - these methods don't exist on CellImmutable
        // locked.insert(...) // compile error
    }

    #[test]
    fn test_cellmap_same_cell_returned() {
        let map = CellMap::<String, i32>::new();

        let cell1 = map.get(&"a".to_string()).materialize();
        let cell2 = map.get(&"a".to_string()).materialize();

        // Both should reflect same updates
        map.insert("a".to_string(), 42);

        assert_eq!(cell1.get(), Some(42));
        assert_eq!(cell2.get(), Some(42));
    }

    #[test]
    fn concurrent_gets_share_the_single_notified_cell() {
        const READERS: usize = 32;
        let map = CellMap::<String, i32>::new();
        let barrier = Arc::new(Barrier::new(READERS));
        let (tx, rx) = std::sync::mpsc::channel();

        for _ in 0..READERS {
            let map = map.clone();
            let barrier = Arc::clone(&barrier);
            let tx = tx.clone();
            drop(std::thread::spawn(move || {
                barrier.wait();
                let _ = tx.send(map.get(&"key".to_string()).materialize());
            }));
        }
        drop(tx);

        let cells: Vec<_> = rx.into_iter().collect();
        assert_eq!(cells.len(), READERS);
        map.insert("key".to_string(), 42);

        assert!(cells.iter().all(|cell| cell.get() == Some(42)));
    }

    #[test]
    fn test_cellmap_mutable_clone_shares_inner_map() {
        let map = CellMap::<String, i32>::new();
        let map_clone = map.clone();

        map.insert("a".to_string(), 1);
        assert_eq!(map_clone.get_value(&"a".to_string()), Some(1));

        map_clone.insert("b".to_string(), 2);
        assert_eq!(map.get_value(&"b".to_string()), Some(2));
        assert_eq!(map.len().materialize().get(), 2);
        assert_eq!(map_clone.len().materialize().get(), 2);
    }

    #[test]
    fn test_replace_all() {
        let map = CellMap::<String, i32>::new();
        map.insert("a".to_string(), 1);
        map.insert("b".to_string(), 2);
        map.insert("c".to_string(), 3);

        let (tx, rx) = std::sync::mpsc::channel::<MapDiff<String, i32>>();
        let _guard = map.subscribe_diffs(move |diff| {
            let _ = tx.send(diff.clone());
        });

        // Replace: keep "a" (updated), add "d", remove "b" and "c"
        map.replace_all(vec![("a".to_string(), 10), ("d".to_string(), 4)]);

        assert_eq!(map.len().materialize().get(), 2);
        assert_eq!(map.get_value(&"a".to_string()), Some(10));
        assert_eq!(map.get_value(&"d".to_string()), Some(4));
        assert_eq!(map.get_value(&"b".to_string()), None);
        assert_eq!(map.get_value(&"c".to_string()), None);

        // Initial snapshot + replace_all Batch
        let seen: Vec<_> = rx.try_iter().collect();
        assert_eq!(seen.len(), 2);
        assert!(matches!(seen.get(1), Some(MapDiff::Batch { changes }) if changes.len() == 4));
    }

    #[test]
    fn test_replace_all_noop() {
        let map = CellMap::<String, i32>::new();
        map.insert("a".to_string(), 1);

        // Replace with same data
        map.replace_all(vec![("a".to_string(), 1)]);

        assert_eq!(map.len().materialize().get(), 1);
        assert_eq!(map.get_value(&"a".to_string()), Some(1));
    }

    #[test]
    fn test_replace_all_empty() {
        let map = CellMap::<String, i32>::new();
        map.insert("a".to_string(), 1);
        map.insert("b".to_string(), 2);

        map.replace_all(vec![]);

        assert_eq!(map.len().materialize().get(), 0);
        assert_eq!(map.get_value(&"a".to_string()), None);
        assert_eq!(map.get_value(&"b".to_string()), None);
    }

    // Regression: `key_cells` must not grow without bound as distinct keys are
    // observed via `get` and then their cells are dropped. Before the amortized
    // sweep, every distinct key ever passed to `get` left a dangling `WeakCell`
    // slot behind forever — the production OOM in rship.
    #[test]
    fn test_key_cells_pruned_under_churn() {
        let map = CellMap::<u64, u64>::new();

        // Churn a large number of distinct keys: observe each (creating a
        // key_cells slot), drop the observer, then drive mutations.
        for i in 0..KEY_CELL_CHURN {
            let cell = map.get(&i).materialize();
            assert_eq!(cell.get(), None);
            drop(cell); // observer gone → this key's weak now dangles
            map.insert(i, i);
            map.remove(&i);
        }

        // Amortized pruning must have kept key_cells bounded far below the
        // number of distinct keys churned (it would otherwise be ~CHURN).
        let live = map.inner.key_cells.len();
        assert!(
            live < 256,
            "key_cells should stay bounded under churn, got {live} slots after {KEY_CELL_CHURN} keys",
        );
    }

    // Pruning must never evict a still-observed key: a live cell held across a
    // churn storm must keep receiving updates (reinsert re-notify preserved).
    #[test]
    fn test_key_cells_prune_keeps_live_observer() {
        let map = CellMap::<u64, u64>::new();

        // A long-lived observer on a stable key.
        let watched = map.get(&0).materialize();
        let hits = Arc::new(AtomicUsize::new(0));
        let h = hits.clone();
        let _guard = watched.subscribe(move |_| {
            h.fetch_add(1, Ordering::SeqCst);
        });
        let initial = hits.load(Ordering::SeqCst);

        // Churn many other keys to trigger repeated sweeps.
        for i in 1..2_000u64 {
            let cell = map.get(&i).materialize();
            drop(cell);
            map.insert(i, i);
            map.remove(&i);
        }

        // The watched key's live cell survived every sweep and still updates.
        map.insert(0, 99);
        assert_eq!(watched.get(), Some(99));
        assert!(
            hits.load(Ordering::SeqCst) > initial,
            "live observer must still be notified after pruning sweeps",
        );
    }
}
