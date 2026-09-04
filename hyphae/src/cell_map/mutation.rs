//! Mutable `CellMap` construction and mutation paths.

use std::{
    hash::Hash,
    marker::PhantomData,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use dashmap::DashMap;
use parking_lot::Mutex;
use uuid::Uuid;

use crate::{
    cell::{Cell, CellImmutable, CellMutable},
    subscription::SubscriptionGuard,
    traits::{CellValue, Mutable},
};

use super::{CellMap, CellMapInner, MapDiff};

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
