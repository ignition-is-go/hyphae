//! Reactive `HashMap` with per-key observability.
//!
//! `CellMap` wraps a concurrent `HashMap` where each entry can be individually observed.
//! Changes to keys trigger reactive updates to observers.
//!
//! # Write order
//!
//! A write stores its row at once and tells subscribers afterwards: on the
//! writing thread when no batch is open, when the batch commits otherwise.
//! Reads of the map (`get_value`, `contains_key`, `snapshot`, the value
//! `insert` returns, a write hook) therefore see a row as soon as it is
//! written, from any thread, including inside the batch that wrote it. The
//! cells derived from the map (its diffs, per-key observers, length, and
//! every query built on them) see it when it is delivered.
//!
//! For every key, diffs are delivered in the order the rows were stored,
//! whichever threads wrote them and whichever of those held a batch open.
//! Once every writer has returned and propagation has finished, each derived
//! structure equals what it would compute from the map's rows. No writer
//! waits for another's subscribers:
//!
//! - Two batches that write the same key commit together, when the later of
//!   them closes. The one that closes first returns, and its writes are
//!   delivered with the other's. Batches that share no key commit
//!   independently.
//! - A write made outside a batch to a key an open batch has written is
//!   delivered when that batch commits.
//! - A write made outside a batch to a key another thread is delivering
//!   outside a batch returns at once, and that thread delivers it when its
//!   own subscribers have returned.
//! - Any other write made outside a batch is delivered as an unbatched cell
//!   write is: before the write returns, or by the next drain when a batch
//!   is open or a drain is running on the map's tick. Two threads that write
//!   different keys this way run their subscribers at the same time.
//!
//! The length cell counts delivered rows, so it never runs ahead of the
//! diffs. The length, entries, keys and items cells take each value in the
//! order it was computed, also when two threads deliver at once.
//!
//! # Write hooks
//!
//! [`CellMap::register_write_hook`] keeps a secondary index exactly in step
//! with a map. Diff subscribers are deferred while a batch is open; a write
//! hook is not. The contract:
//!
//! - **Coverage.** The hook sees every change to the map's data, from every
//!   mutating method, one [`MapWrite`] per changed key, with the old value on
//!   every update and remove. A write the map suppresses as equal is not
//!   reported. `Initial` diffs passed to `apply_diff_owned` or `apply_batch`
//!   report a `Remove` then an `Insert` for each key they rewrite, because
//!   that is what the map does: a reader of the map sees the key absent in
//!   between, and so does the index.
//! - **Ordering.** The hook runs on the writing thread, under the write lock
//!   of the shard that holds the key, after the map holds the new value and
//!   before the write returns, whether or not a batch is open. A read of that
//!   key waits on the same lock, so for a single key the map and the index
//!   never disagree: a reader that sees a row in the map then finds it in the
//!   index, and a reader that finds a row in the index then reads it from the
//!   map. For one key, hook calls arrive in the order the map applies the
//!   writes. Different keys are not ordered.
//! - **Seeding.** Registration write-locks every shard, replays each current
//!   entry as a `MapWrite::Insert`, and installs the hook before releasing
//!   them. Every concurrent write is in the replay or in a later call, never
//!   both. Writers to the map wait for the replay: measured about 4 ms for
//!   100,000 rows and 29 to 34 ms for 500,000 rows with a hook that fills a
//!   `DashMap`.
//! - **Restrictions.** The hook must not read or write the same map, or
//!   register or drop a write hook on it: those calls take the shard lock the
//!   hook already holds and deadlock. Keep it short; it blocks readers and
//!   writers of the keys in its shard. While any hook is registered, the
//!   equality check that suppresses equal writes runs first under the shard
//!   read lock, which blocks other writers of the shard but not its readers,
//!   and runs again under the write lock only if the shard was written in
//!   between. Old values and
//!   removed entries are dropped after the lock is released, as without a
//!   hook.
//! - **Cost.** With no hook, a write pays a relaxed atomic load under the
//!   lock it already holds. With hooks, it also pays the read-locked
//!   equality check, a per-shard counter, and the callbacks.
//! - **Lifetime.** Dropping the returned guard removes the hook. Several hooks
//!   may be registered; each sees every write.
//!
//! ```
//! use std::{collections::{HashMap, HashSet}, sync::{Arc, Mutex}};
//! use hyphae::{CellMap, cell_map::MapWrite};
//!
//! #[derive(Clone, Debug, PartialEq)]
//! struct Task { owner: &'static str }
//!
//! let tasks = CellMap::<u32, Task>::new();
//! tasks.insert(1, Task { owner: "ada" });
//!
//! // owner -> task ids, kept exact by the hook.
//! let by_owner: Arc<Mutex<HashMap<&'static str, HashSet<u32>>>> = Arc::default();
//! let index = by_owner.clone();
//! let _guard = tasks.register_write_hook(move |write| {
//!     let mut index = index.lock().unwrap();
//!     let (key, old, new) = match write {
//!         MapWrite::Insert { key, value } => (key, None, Some(value)),
//!         MapWrite::Update { key, old, new } => (key, Some(old), Some(new)),
//!         MapWrite::Remove { key, old } => (key, Some(old), None),
//!     };
//!     if let Some(old) = old {
//!         index.entry(old.owner).or_default().remove(key);
//!     }
//!     if let Some(new) = new {
//!         index.entry(new.owner).or_default().insert(*key);
//!     }
//! });
//!
//! tasks.insert(2, Task { owner: "ada" });
//! tasks.insert(1, Task { owner: "lin" });
//! let index = by_owner.lock().unwrap();
//! assert_eq!(index["ada"], HashSet::from([2]));
//! assert_eq!(index["lin"], HashSet::from([1]));
//! ```

use std::{
    hash::Hash,
    marker::PhantomData,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

use dashmap::{DashMap, mapref::entry::Entry};
use parking_lot::Mutex;

use crate::{
    cell::{Cell, CellImmutable, CellMutable, WeakCell},
    pipeline::{Definite, Materialize},
    signal::Signal,
    subscription::SubscriptionGuard,
    traits::{CellValue, Mutable, Watchable},
};

mod diff;
mod mutation;
mod projection;
mod write_hook;

pub use diff::MapDiff;
use projection::{EntryProjection, KeyProjection, ProjectionOwner, ValueProjection};
pub use write_hook::MapWrite;

struct PointCellSlot<V: CellValue> {
    state: Mutex<PointCellState<V>>,
}

struct PointCellState<V: CellValue> {
    cell: Option<WeakCell<Option<V>, CellMutable>>,
    revision: usize,
}

impl<V: CellValue> PointCellSlot<V> {
    const fn new() -> Self {
        Self {
            state: Mutex::new(PointCellState {
                cell: None,
                revision: 0,
            }),
        }
    }

    fn snapshot(&self) -> (usize, Option<Cell<Option<V>, CellMutable>>) {
        let state = self.state.lock();
        (
            state.revision,
            state.cell.as_ref().and_then(WeakCell::upgrade),
        )
    }

    fn publish(
        &self,
        revision: usize,
        candidate: &Cell<Option<V>, CellMutable>,
    ) -> Option<Cell<Option<V>, CellMutable>> {
        let mut state = self.state.lock();
        if let Some(existing) = state.cell.as_ref().and_then(WeakCell::upgrade) {
            return Some(existing);
        }
        if state.revision != revision {
            return None;
        }
        state.cell = Some(candidate.downgrade());
        drop(state);
        Some(candidate.clone())
    }

    fn changed(&self) -> Option<Cell<Option<V>, CellMutable>> {
        let mut state = self.state.lock();
        state.revision = state.revision.wrapping_add(1);
        state.cell.as_ref().and_then(WeakCell::upgrade)
    }

    fn is_alive(&self) -> bool {
        self.state
            .lock()
            .cell
            .as_ref()
            .is_some_and(WeakCell::is_alive)
    }
}

/// Shard count for a map's stores. `DashMap` defaults to four shards per CPU,
/// which costs 128 bytes per shard per map and one allocation per shard on
/// every iteration. Most maps hold a handful of rows, and the shared diff cell
/// every write sets bounds write throughput well below what four shards
/// sustain.
const SHARDS: usize = 4;

pub(crate) struct CellMapInner<K, V>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    /// The actual data storage.
    pub(crate) data: DashMap<K, V>,
    /// Cached per-key observation cells.
    ///
    /// Slots hold only `WeakCell`s: once every strong handle a caller holds from
    /// `get(k)` drops, the weak dangles but its slot lingers here. Left
    /// unchecked this is an unbounded leak on maps with high key churn (every
    /// distinct key ever observed keeps a slot forever). [`maybe_prune_key_cells`]
    /// amortizes a sweep of dead weaks across mutations to keep it bounded.
    key_cells: OnceLock<DashMap<K, Arc<PointCellSlot<V>>>>,
    key_cell_slots: AtomicUsize,
    /// Mutation counter driving amortized pruning of dead `key_cells` weaks.
    prune_ops: AtomicUsize,
    /// Synchronous hooks run under the shard lock of every data write.
    write_hooks: write_hook::WriteHooks<K, V>,
    /// Cell for diff notifications.
    pub(crate) diffs_cell: Cell<MapDiff<K, V>, CellMutable>,
    /// Cell for length.
    len_cell: Cell<usize, CellMutable>,
    /// Rows the length cell has been told about: rows added less rows
    /// removed over every delivered write.
    delivered_len: std::sync::atomic::AtomicIsize,
    /// Sets the length cell to each count in the order they were reached.
    len: crate::scheduler::order::Sequenced<usize>,
    /// The order rows are stored in and their diffs delivered in.
    order: Arc<crate::scheduler::order::WriteOrder>,
    /// Diffs staged in open batches and not yet delivered.
    staged: Mutex<Vec<mutation::StagedDiff<K, V>>>,
    /// Subscription guards owned by this map (dropped when map drops).
    owned: Mutex<Vec<SubscriptionGuard>>,
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
    fn install_output<S, O>(
        &self,
        source: &Cell<S, CellMutable>,
        initial: O,
        replay: OutputReplay,
        update: impl Fn(&S) -> Option<O> + Send + Sync + 'static,
    ) -> Cell<O, CellImmutable>
    where
        S: CellValue,
        O: CellValue,
    {
        let cell = Cell::new(initial);

        let weak_cell = cell.downgrade();
        let map_keepalive = self.inner.clone();
        let outputs = crate::scheduler::order::Sequenced::default();
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
            if let Signal::Value(value) = signal {
                // Two threads can deliver diffs of different keys at once.
                // Each output is computed and takes its turn in one step,
                // so the cell ends on the one computed last.
                outputs.publish(|| update(value.as_ref()), |output| cell.set(output));
            }
        });
        cell.own(guard);
        cell.lock()
    }

    fn install_projection<P, O>(
        &self,
        projection: P,
        initial: O,
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
            OutputReplay::Skip,
            move |diff| projection.with(|state| update(state, diff)),
        )
    }

    /// The scheduling domain this map belongs to: the one that was ambient
    /// where the map was created. Its diff, length and per-key cells all live
    /// there.
    #[must_use]
    pub fn domain(&self) -> crate::scheduler::Domain {
        self.inner.diffs_cell.domain()
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
    pub fn get(&self, key: &K) -> impl Materialize<Option<V>, Definite> + use<K, V, M> {
        let key_cells = self
            .inner
            .key_cells
            .get_or_init(|| DashMap::with_shard_amount(SHARDS));
        let reserved = key_cells.get(key).map(|slot| slot.value().clone());
        let slot = reserved.unwrap_or_else(|| match key_cells.entry(key.clone()) {
            Entry::Occupied(slot) => slot.get().clone(),
            Entry::Vacant(entry) => {
                let slot = Arc::new(PointCellSlot::new());
                let inserted = entry.insert(slot.clone());
                self.inner.key_cell_slots.fetch_add(1, Ordering::Relaxed);
                drop(inserted);
                slot
            }
        });
        let (mut revision, cached) = slot.snapshot();
        if let Some(cached) = cached {
            return cached.lock();
        }
        let mut current = self.inner.data.get(key).map(|entry| entry.value().clone());
        #[cfg(test)]
        point_cell_race_tests::after_seed_read();
        // The observer belongs to the map, not to whoever asks first: it is
        // cached and handed to every later caller, in any domain.
        let home = self.inner.diffs_cell.inner.domain.as_ref();
        let _scope =
            crate::scheduler::domain::in_use().then(|| crate::scheduler::domain::enter(home));
        loop {
            let candidate = Cell::new(current);
            // NOTE(cx): Cloning may mutate another key. Only this key's writes
            // invalidate the seed, and user code runs outside the slot mutex.
            if let Some(cell) = slot.publish(revision, &candidate) {
                return cell.lock();
            }
            drop(candidate);
            let (next_revision, cached) = slot.snapshot();
            if let Some(cached) = cached {
                return cached.lock();
            }
            revision = next_revision;
            current = self.inner.data.get(key).map(|entry| entry.value().clone());
        }
    }

    /// Build an observable pipeline of all entries.
    ///
    /// Returns a derived cell that maintains entries incrementally via diffs.
    /// The initial value is computed from the current map state, then updates
    /// are applied incrementally as O(1) operations per diff.
    #[must_use]
    pub fn entries(&self) -> impl Materialize<Vec<(K, V)>, Definite> + use<K, V, M> {
        let initial = self.snapshot();
        self.install_projection(
            EntryProjection::from_entries(initial.clone()),
            initial,
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
    #[must_use]
    pub fn items(&self) -> impl Materialize<Vec<V>, Definite> + use<K, V, M> {
        let projection = ValueProjection::from_entries(self.snapshot());
        let initial = projection.items();
        self.install_projection(projection, initial, |projection, diff| {
            projection.apply_diff(diff);
            Some(projection.items())
        })
    }

    /// Build an observable pipeline of all keys.
    #[must_use]
    pub fn keys(&self) -> impl Materialize<Vec<K>, Definite> + use<K, V, M> {
        let projection = KeyProjection::from_keys(self.keys_snapshot());
        let initial = projection.keys();
        self.install_projection(projection, initial, |projection, diff| {
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
        for shard in self.inner.data.shards() {
            let table = shard.read();
            // SAFETY: the shard's read guard outlives the iterator and every
            // bucket reference, and no writer can modify the table under it.
            for bucket in unsafe { table.iter() } {
                let (key, value) = unsafe { bucket.as_ref() };
                f(key, value.get());
            }
        }
    }

    /// Map every entry into a `Vec`, one shard read lock at a time.
    ///
    /// `DashMap::iter` allocates an `Arc` per shard and clones it for every
    /// entry, which makes it about ten times slower than walking the shards.
    fn collect<T>(&self, mut f: impl FnMut(&K, &V) -> T) -> Vec<T> {
        let mut out = Vec::with_capacity(self.inner.data.len());
        self.for_each(|key, value| out.push(f(key, value)));
        out
    }

    /// Get a point-in-time snapshot of all entries (non-reactive).
    ///
    /// Unlike `entries()`, this does NOT create a Cell or subscribe to changes.
    /// Use this for one-shot reads where you don't need live updates.
    #[must_use]
    pub fn snapshot(&self) -> Vec<(K, V)> {
        self.collect(|key, value| (key.clone(), value.clone()))
    }

    /// Return the current values without creating a reactive projection.
    #[must_use]
    pub fn items_snapshot(&self) -> Vec<V> {
        self.collect(|_, value| value.clone())
    }

    /// Return the current keys without creating a reactive projection.
    #[must_use]
    pub fn keys_snapshot(&self) -> Vec<K> {
        self.collect(|key, _| key.clone())
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
        let callback = Arc::new(callback);
        let initializing = Arc::new(AtomicBool::new(true));
        let pending = Arc::new(Mutex::new(Vec::new()));

        // Subscribe before reading the snapshot. Mutations racing construction
        // are buffered until the snapshot has been delivered, then replayed in
        // notification order.
        let map_keepalive = self.inner.clone();
        let diffs = self.diffs().materialize();
        let first = Arc::new(AtomicBool::new(true));
        let callback_for_diffs = callback.clone();
        let initializing_for_diffs = initializing.clone();
        let pending_for_diffs = pending.clone();
        let guard = diffs.subscribe(move |signal| {
            let _ = &map_keepalive;
            if first.swap(false, Ordering::SeqCst) {
                return;
            }
            let Signal::Value(diff) = signal else { return };
            if initializing_for_diffs.load(Ordering::Acquire) {
                let mut queued = pending_for_diffs.lock();
                if initializing_for_diffs.load(Ordering::Relaxed) {
                    queued.push(diff.as_ref().clone());
                    return;
                }
                drop(queued);
            }
            callback_for_diffs(diff.as_ref());
        });

        let entries = self.collect(|key, value| (key.clone(), value.clone()));
        callback(&MapDiff::Initial { entries });
        loop {
            let queued = {
                let mut queued = pending.lock();
                if queued.is_empty() {
                    initializing.store(false, Ordering::Release);
                    break;
                }
                std::mem::take(&mut *queued)
            };
            for diff in queued {
                callback(&diff);
            }
        }
        guard
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
        self.collect(|key, _| key.clone())
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
        self.collect(|key, value| (key.clone(), value.clone()))
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

#[cfg(test)]
mod point_cell_race_tests;

#[cfg(test)]
mod owned_guard_tests;

#[cfg(test)]
mod lazy_point_cache_tests;

#[cfg(test)]
mod write_hook_tests;
