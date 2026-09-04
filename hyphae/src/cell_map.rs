//! Reactive `HashMap` with per-key observability.
//!
//! `CellMap` wraps a concurrent `HashMap` where each entry can be individually observed.
//! Changes to keys trigger reactive updates to observers.

use std::{
    hash::Hash,
    marker::PhantomData,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
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

mod diff;
mod projection;

pub use diff::MapDiff;
use projection::{EntryProjection, KeyProjection, ProjectionOwner, ValueProjection};

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

#[derive(Clone, Copy, PartialEq, Eq)]
enum OutputReplay {
    Apply,
    Skip,
}

impl<K, V, M> CellMap<K, V, M>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    #[track_caller]
    fn install_output<S, O>(
        &self,
        source: &Cell<S, CellMutable>,
        initial: O,
        name: Option<&str>,
        replay: OutputReplay,
        update: impl Fn(&S) -> Option<O> + Send + Sync + 'static,
    ) -> Cell<O, CellImmutable>
    where
        S: CellValue,
        O: CellValue,
    {
        let cell = Cell::new(initial);
        if let (Some(name), Some(map_name)) = (name, self.inner.name.lock().as_ref()) {
            drop(cell.clone().with_name(format!("{map_name}::{name}")));
        }

        let weak_cell = cell.downgrade();
        let map_keepalive = self.inner.clone();
        let skip_replay = matches!(replay, OutputReplay::Skip).then(|| AtomicBool::new(true));
        let guard = source.subscribe(move |signal| {
            let _ = &map_keepalive;
            if skip_replay
                .as_ref()
                .is_some_and(|first| first.swap(false, Ordering::SeqCst))
            {
                return;
            }
            let Some(cell) = weak_cell.upgrade() else {
                return;
            };
            if let Signal::Value(value) = signal
                && let Some(output) = update(value.as_ref())
            {
                cell.set(output);
            }
        });
        cell.own(guard);
        cell.lock()
    }

    #[track_caller]
    fn install_projection<P, O>(
        &self,
        projection: P,
        initial: O,
        name: &str,
        update: impl Fn(&mut P, &MapDiff<K, V>) -> Option<O> + Send + Sync + 'static,
    ) -> Cell<O, CellImmutable>
    where
        P: Send + 'static,
        O: CellValue,
    {
        let projection = ProjectionOwner::new(projection);
        self.install_output(
            &self.inner.diffs_cell,
            initial,
            Some(name),
            OutputReplay::Skip,
            move |diff| projection.with(|state| update(state, diff)),
        )
    }

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
        let initial = self.snapshot();
        self.install_projection(
            EntryProjection::from_entries(initial.clone()),
            initial,
            "entries",
            |projection, diff| {
                projection.apply_diff(diff);
                Some(projection.entries())
            },
        )
    }

    /// Build an observable pipeline of all values.
    ///
    /// This maintains its own diff-driven projection to avoid forcing an
    /// intermediate entries materialization on hot value-only paths.
    #[track_caller]
    #[must_use]
    pub fn items(&self) -> impl Materialize<Vec<V>, Definite> + use<K, V, M> {
        let projection = ValueProjection::from_entries(self.snapshot());
        let initial = projection.items();
        self.install_projection(projection, initial, "items", |projection, diff| {
            projection.apply_diff(diff);
            Some(projection.items())
        })
    }

    /// Build an observable pipeline of all keys.
    #[track_caller]
    #[must_use]
    pub fn keys(&self) -> impl Materialize<Vec<K>, Definite> + use<K, V, M> {
        let projection = KeyProjection::from_keys(self.keys_snapshot());
        let initial = projection.keys();
        self.install_projection(projection, initial, "keys", |projection, diff| {
            projection.apply_diff(diff).then(|| projection.keys())
        })
    }

    /// Get an observable Cell of the map size.
    ///
    /// This is the preferred reactive count operator because it reuses the
    /// internally maintained length cell instead of materializing entries.
    #[must_use]
    pub fn size(&self) -> impl Materialize<usize, Definite> + use<K, V, M> {
        self.install_output(
            &self.inner.len_cell,
            self.inner.data.len(),
            None,
            OutputReplay::Apply,
            |len| Some(*len),
        )
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
mod tests;
