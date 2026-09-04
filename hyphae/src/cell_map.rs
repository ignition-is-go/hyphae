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
mod mutation;
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
