use std::{
    cell::RefCell,
    hash::Hash,
    marker::PhantomData,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
};

use dashmap::DashMap;
use parking_lot::Mutex;

use super::{
    CellMap, CellMapInner, MapDiff,
    publish::{Held, InFlight, joins_settle, write_in_order},
};
use crate::{
    cell::{Cell, CellImmutable, CellMutable},
    subscription::SubscriptionGuard,
    traits::{CellValue, Mutable},
};

/// The diff a mutation emits: one change, a batch of changes, or every entry.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Shape {
    One,
    Batch,
    Initial,
}

/// A mutation that publishes when a settle applies its request.
struct Later<K> {
    pass: u64,
    /// The keys it changed, each once, in the order it first changed them.
    keys: Vec<K>,
    shape: Shape,
}

/// How one mutation tells subscribers what it changed.
///
/// A mutation that joins the settle in progress, and changes no key that has
/// a publish request queued, writes the key cells, the diff, and the length
/// as it goes. Any other mutation records the keys it changes and queues one
/// publish request when this is dropped. See the `publish` module.
struct Tell<'a, K, V>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    map: &'a CellMap<K, V, CellMutable>,
    flight: &'a RefCell<InFlight<K, V>>,
    later: Option<Later<K>>,
    /// The changes of a batch diff, collected only when writing at once.
    changes: Vec<MapDiff<K, V>>,
}

impl<'a, K, V> Tell<'a, K, V>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    /// `in_flight` says whether the mutation may change a key that has a
    /// publish request queued. It is asked only when some key has one.
    fn new(
        map: &'a CellMap<K, V, CellMutable>,
        flight: &'a RefCell<InFlight<K, V>>,
        in_flight: impl FnOnce(&InFlight<K, V>) -> bool,
    ) -> Self {
        let now = joins_settle(&map.inner.diffs_cell) && {
            let held = flight.borrow();
            held.is_empty() || !in_flight(&held)
        };
        let later = (!now).then(|| Later {
            pass: InFlight::begin(flight),
            keys: Vec::new(),
            shape: Shape::One,
        });
        Self {
            map,
            flight,
            later,
            changes: Vec::new(),
        }
    }

    const fn is_now(&self) -> bool {
        self.later.is_none()
    }

    /// The mutation changed the stored value of `key` from `old` to `new`.
    fn key(&mut self, key: &K, old: Option<&V>, new: Option<&V>) {
        InFlight::resize(self.flight, old.is_some(), new.is_some());
        match &mut self.later {
            None => {
                if let Some(cell) = self.map.observed_key_cell(key) {
                    cell.set(new.cloned());
                }
            }
            Some(later) => {
                if old != new && InFlight::touch(self.flight, later.pass, key, old) {
                    later.keys.push(key.clone());
                    // A key cell being created from the stored value it read
                    // before this change must read again.
                    drop(self.map.observed_key_cell(key));
                }
            }
        }
    }

    /// Add one change to the batch diff this mutation emits.
    fn change(&mut self, change: impl FnOnce() -> MapDiff<K, V>) {
        if self.is_now() {
            self.changes.push(change());
        }
    }

    /// Emit the collected changes as one batch, if there are any.
    fn emit_batch(&mut self) {
        let changes = std::mem::take(&mut self.changes);
        match &mut self.later {
            None if changes.is_empty() => {}
            None => self.map.inner.diffs_cell.set(MapDiff::Batch { changes }),
            Some(later) => later.shape = later.shape.max_with(Shape::Batch),
        }
    }

    fn emit(&mut self, shape: Shape, diff: impl FnOnce() -> MapDiff<K, V>) {
        match &mut self.later {
            None => self.map.inner.diffs_cell.set(diff()),
            Some(later) => later.shape = later.shape.max_with(shape),
        }
    }

    /// The mutation replaced every entry, so its diff must carry them all.
    const fn resets(&mut self) {
        if let Some(later) = &mut self.later {
            later.shape = Shape::Initial;
        }
    }

    /// Write the length, if subscribers hold another.
    fn len(&self) {
        if self.is_now()
            && let Some(len) = InFlight::tell_len(self.flight)
        {
            self.map.inner.len_cell.set(len);
        }
    }
}

impl Shape {
    /// `Initial` replaces everything, and a batch can hold one change.
    const fn max_with(self, other: Self) -> Self {
        match (self, other) {
            (Self::Initial, _) | (_, Self::Initial) => Self::Initial,
            (Self::Batch, _) | (_, Self::Batch) => Self::Batch,
            (Self::One, Self::One) => Self::One,
        }
    }
}

impl<K, V> Drop for Tell<'_, K, V>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    /// Queue the publish request. On drop, so that a mutation that returns
    /// early or panics still publishes the keys it changed.
    fn drop(&mut self) {
        let Some(later) = self.later.take() else {
            return;
        };
        if later.keys.is_empty() && later.shape != Shape::Initial {
            return;
        }
        crate::settle::submit_write(
            crate::settle::SettleKey::Shared,
            Box::new(Publish {
                map: Arc::downgrade(&self.map.inner),
                keys: later.keys,
                shape: later.shape,
            }),
        );
    }
}

/// A queued request to tell a map's subscribers what is stored for `keys`.
/// Holds the map weakly: a map that nothing else holds has nobody to tell.
struct Publish<K, V>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    map: std::sync::Weak<CellMapInner<K, V>>,
    keys: Vec<K>,
    shape: Shape,
}

impl<K, V> crate::settle::Write for Publish<K, V>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    fn apply(self: Box<Self>) {
        if let Some(inner) = self.map.upgrade() {
            CellMap {
                inner,
                _marker: PhantomData::<CellMutable>,
            }
            .publish(self.keys, self.shape);
        }
    }
}

/// Whether applying `diff` may change a key that has a publish request
/// queued. `held` is not empty.
fn touches_in_flight<K, V>(held: &InFlight<K, V>, diff: &MapDiff<K, V>) -> bool
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    match diff {
        MapDiff::Initial { .. } => true,
        MapDiff::Insert { key, .. } | MapDiff::Remove { key, .. } | MapDiff::Update { key, .. } => {
            held.holds(key)
        }
        MapDiff::Batch { changes } => changes.iter().any(|change| touches_in_flight(held, change)),
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
        Self::from_entries(Vec::new())
    }

    /// Create a `CellMap` that already holds `entries`.
    ///
    /// Nothing can observe a map before it exists, so this makes no write and
    /// takes no part in a settle. Use it instead of `new` followed by a write
    /// when the caller holds a lock that a subscriber callback also takes.
    #[track_caller]
    #[must_use]
    pub fn from_entries(entries: Vec<(K, V)>) -> Self {
        let data = DashMap::new();
        for (key, value) in &entries {
            data.insert(key.clone(), value.clone());
        }
        let len = data.len();
        let diffs_cell = Cell::new(MapDiff::Initial { entries });
        // Diffs are events: a subscriber that accumulates them must see every
        // one, in order. Diffs that arrive in one settle compose into one batch.
        let diffs_cell = diffs_cell.combine_with(MapDiff::compose);
        let len_cell = Cell::new(len);
        // The length and each key cell are written beside the diff cell, so
        // they take their height from it.
        len_cell.own(SubscriptionGuard::dependency(&diffs_cell));

        // Mark len_cell as owned by diffs_cell so it doesn't appear as an orphan root

        Self {
            inner: Arc::new(CellMapInner {
                data,
                write_order: InFlight::new(len),
                key_cells: OnceLock::new(),
                key_cell_slots: AtomicUsize::new(0),
                prune_ops: AtomicUsize::new(0),
                diffs_cell,
                len_cell,
                owned: Mutex::new(Vec::new()),
                name: Mutex::new(None),
            }),
            _marker: PhantomData,
        }
    }

    /// Own a subscription guard, keeping it alive as long as this `CellMap` exists.
    pub fn own(&self, guard: SubscriptionGuard) {
        // The diff cell is what this map publishes through, so the guard's
        // source is a dependency of the diff cell: a diff published while that
        // source is firing joins the same settle.
        self.inner
            .diffs_cell
            .own(SubscriptionGuard::dependency_on(guard.source()));
        self.inner.owned.lock().push(guard);
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
        let Some(key_cells) = self.inner.key_cells.get() else {
            return;
        };
        let len = self.inner.key_cell_slots.load(Ordering::Relaxed);
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
        key_cells.retain(|_, slot| {
            if Arc::strong_count(slot) > 1 || slot.is_alive() {
                true
            } else {
                self.inner.key_cell_slots.fetch_sub(1, Ordering::Relaxed);
                false
            }
        });
    }

    // Upgrading inside this helper releases the shard guard before any callback.
    // Observers may materialize another key in this same map, including a key
    // sharing the shard. Holding a Ref across `set` would deadlock that lookup.
    fn observed_key_cell(&self, key: &K) -> Option<Cell<Option<V>, CellMutable>> {
        let slot = self
            .inner
            .key_cells
            .get()?
            .get(key)
            .map(|slot| slot.value().clone());
        slot.and_then(|slot| slot.changed())
    }

    /// Insert a key-value pair, returning the old value if present.
    pub fn insert(&self, key: K, value: V) -> Option<V> {
        write_in_order(&self.inner.write_order, |flight| {
            let mut tell = Tell::new(self, flight, |held| held.holds(&key));
            self.maybe_prune_key_cells();
            let old = self.inner.data.insert(key.clone(), value.clone());

            // No-op update: same key/value should not emit a diff or notify observers.
            if old.as_ref().is_some_and(|old_value| old_value == &value) {
                return old;
            }

            tell.key(&key, old.as_ref(), Some(&value));
            let previous = old.clone();
            tell.emit(Shape::One, || match previous {
                Some(old_value) => MapDiff::Update {
                    key,
                    old_value,
                    new_value: value,
                },
                None => MapDiff::Insert { key, value },
            });
            tell.len();

            old
        })
    }

    /// Insert multiple key-value pairs and emit a single batch diff.
    pub fn insert_many(&self, entries: Vec<(K, V)>) {
        write_in_order(&self.inner.write_order, |flight| {
            if entries.is_empty() {
                return;
            }
            let mut tell = Tell::new(self, flight, |held| {
                entries.iter().any(|(key, _)| held.holds(key))
            });
            self.maybe_prune_key_cells();

            for (key, value) in entries {
                let old = self.inner.data.insert(key.clone(), value.clone());
                if old.as_ref().is_some_and(|old_value| old_value == &value) {
                    continue;
                }
                tell.key(&key, old.as_ref(), Some(&value));
                tell.change(|| match old {
                    Some(old_value) => MapDiff::Update {
                        key,
                        old_value,
                        new_value: value,
                    },
                    None => MapDiff::Insert { key, value },
                });
            }

            tell.emit_batch();
            tell.len();
        });
    }

    /// Remove a key, returning the old value if present.
    pub fn remove(&self, key: &K) -> Option<V> {
        write_in_order(&self.inner.write_order, |flight| {
            let mut tell = Tell::new(self, flight, |held| held.holds(key));
            self.maybe_prune_key_cells();
            let (key, old_value) = self.inner.data.remove(key)?;

            if tell.is_now() {
                // The key cell is written after the diff and the length.
                InFlight::resize(flight, true, false);
                let observed = self.observed_key_cell(&key);
                let previous = old_value.clone();
                tell.emit(Shape::One, || MapDiff::Remove {
                    key,
                    old_value: previous,
                });
                tell.len();
                if let Some(cell) = observed {
                    cell.set(None);
                }
            } else {
                tell.key(&key, Some(&old_value), None);
            }

            Some(old_value)
        })
    }

    /// Remove multiple keys and emit a single batch diff.
    pub fn remove_many(&self, keys: Vec<K>) {
        write_in_order(&self.inner.write_order, |flight| {
            if keys.is_empty() {
                return;
            }
            let mut tell = Tell::new(self, flight, |held| keys.iter().any(|key| held.holds(key)));
            self.maybe_prune_key_cells();

            let original_len = self.inner.data.len();
            let mut removed = 0usize;
            for key in keys {
                if let Some((key, old_value)) = self.inner.data.remove(&key) {
                    removed = removed.saturating_add(1);
                    tell.key(&key, Some(&old_value), None);
                    tell.change(|| MapDiff::Remove { key, old_value });
                }
            }

            if removed == 0 {
                return;
            }

            if self.inner.data.is_empty() && removed == original_len {
                tell.emit(Shape::Initial, || MapDiff::Initial {
                    entries: Vec::new(),
                });
            } else {
                tell.emit_batch();
            }
            tell.len();
        });
    }

    /// Replace all entries atomically, emitting a single `Batch` diff.
    ///
    /// Removes keys not in `entries`, inserts/updates keys that are.
    /// Skips no-op updates (existing key with equal value).
    /// Emits `MapDiff::Batch` with the actual changes so downstream
    /// subscribers see one atomic replacement instead of N individual diffs.
    pub fn replace_all(&self, entries: Vec<(K, V)>) {
        write_in_order(&self.inner.write_order, |flight| {
            let mut tell = Tell::new(self, flight, |_| true);
            self.maybe_prune_key_cells();
            let new_keys: std::collections::HashSet<&K> = entries.iter().map(|(k, _)| k).collect();

            // Remove keys not in new set
            let keys_to_remove: Vec<K> = self
                .inner
                .data
                .iter()
                .filter(|r| !new_keys.contains(r.key()))
                .map(|r| r.key().clone())
                .collect();
            drop(new_keys);

            for key in &keys_to_remove {
                if let Some((key, old_value)) = self.inner.data.remove(key) {
                    tell.key(&key, Some(&old_value), None);
                    tell.change(|| MapDiff::Remove { key, old_value });
                }
            }

            // Insert/update entries
            for (key, value) in entries {
                let old = self.inner.data.insert(key.clone(), value.clone());
                if old.as_ref().is_some_and(|old_value| old_value == &value) {
                    continue;
                }
                tell.key(&key, old.as_ref(), Some(&value));
                tell.change(|| match old {
                    Some(old_value) => MapDiff::Update {
                        key,
                        old_value,
                        new_value: value,
                    },
                    None => MapDiff::Insert { key, value },
                });
            }

            tell.emit_batch();
            tell.len();
        });
    }

    /// Replace the stored entries with `entries`, as `MapDiff::Initial` asks.
    fn store_initial(&self, tell: &mut Tell<'_, K, V>, entries: &[(K, V)]) {
        let stale_keys: Vec<K> = self.inner.data.iter().map(|r| r.key().clone()).collect();
        for key in stale_keys {
            let old = self.inner.data.remove(&key).map(|(_, value)| value);
            tell.key(&key, old.as_ref(), None);
        }
        for (key, value) in entries {
            let old = self.inner.data.insert(key.clone(), value.clone());
            tell.key(key, old.as_ref(), Some(value));
        }
        tell.resets();
    }

    /// Store what one diff other than a batch says. False when the diff
    /// changes nothing and must not be emitted.
    fn store_one(&self, tell: &mut Tell<'_, K, V>, diff: &MapDiff<K, V>) -> bool {
        match diff {
            MapDiff::Initial { entries } => self.store_initial(tell, entries),
            MapDiff::Insert { key, value } => {
                let old = self.inner.data.insert(key.clone(), value.clone());
                tell.key(key, old.as_ref(), Some(value));
            }
            MapDiff::Remove { key, .. } => {
                let old = self.inner.data.remove(key).map(|(_, value)| value);
                tell.key(key, old.as_ref(), None);
            }
            MapDiff::Update { key, new_value, .. } => {
                if self
                    .inner
                    .data
                    .get(key)
                    .is_some_and(|existing| existing.value() == new_value)
                {
                    return false;
                }
                let old = self.inner.data.insert(key.clone(), new_value.clone());
                tell.key(key, old.as_ref(), Some(new_value));
            }
            MapDiff::Batch { .. } => return false,
        }
        true
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
        let diff = match diff {
            MapDiff::Batch { changes } => return self.apply_batch(changes),
            diff => diff,
        };
        write_in_order(&self.inner.write_order, |flight| {
            let mut tell = Tell::new(self, flight, |held| touches_in_flight(held, &diff));
            self.maybe_prune_key_cells();
            if !self.store_one(&mut tell, &diff) {
                return;
            }
            tell.len();
            let shape = if matches!(diff, MapDiff::Initial { .. }) {
                Shape::Initial
            } else {
                Shape::One
            };
            tell.emit(shape, || diff);
        });
    }

    /// Apply a batch of diffs and emit them as one `MapDiff::Batch`.
    pub fn apply_batch(&self, changes: Vec<MapDiff<K, V>>) {
        fn apply_one<K, V>(
            map: &CellMap<K, V, CellMutable>,
            tell: &mut Tell<'_, K, V>,
            diff: MapDiff<K, V>,
        ) -> Option<MapDiff<K, V>>
        where
            K: Hash + Eq + CellValue,
            V: CellValue,
        {
            if let MapDiff::Batch { changes } = diff {
                let applied: Vec<_> = changes
                    .into_iter()
                    .filter_map(|change| apply_one(map, tell, change))
                    .collect();
                return (!applied.is_empty()).then_some(MapDiff::Batch { changes: applied });
            }
            map.store_one(tell, &diff).then_some(diff)
        }

        write_in_order(&self.inner.write_order, |flight| {
            if changes.is_empty() {
                return;
            }
            let mut tell = Tell::new(self, flight, |held| {
                changes.iter().any(|change| touches_in_flight(held, change))
            });
            self.maybe_prune_key_cells();
            let applied_changes: Vec<_> = changes
                .into_iter()
                .filter_map(|change| apply_one(self, &mut tell, change))
                .collect();
            if applied_changes.is_empty() {
                return;
            }
            tell.len();
            tell.emit(Shape::Batch, || MapDiff::Batch {
                changes: applied_changes,
            });
        });
    }

    /// Tell subscribers the step from what they hold to what is stored, for
    /// each key of one queued publish request. A settle calls this while it
    /// seeds a run.
    fn publish(&self, keys: Vec<K>, shape: Shape) {
        let flight = self.inner.write_order.lock();
        if !joins_settle(&self.inner.diffs_cell) {
            // The settling thread has a batch of its own open: it called
            // `flush` inside one. A diff written now would wait in that batch
            // and arrive behind later ones. The request waits there instead,
            // and tells what is stored when it is applied.
            crate::settle::submit_write(
                crate::settle::SettleKey::Shared,
                Box::new(Publish {
                    map: Arc::downgrade(&self.inner),
                    keys,
                    shape,
                }),
            );
            return;
        }
        let stored = |key: &K| self.inner.data.get(key).map(|entry| entry.value().clone());

        if shape == Shape::Initial {
            // The subscribers are sent every stored entry, so they hold the
            // stored value of every key, including the keys other requests
            // have yet to publish.
            InFlight::reset(&flight, stored, |key, live| {
                if let Some(cell) = self.observed_key_cell(key) {
                    cell.set(live.clone());
                }
            });
        }

        let mut changes = Vec::new();
        for key in keys {
            let live = stored(&key);
            let Some(Held(told)) = InFlight::land(&flight, &key, live.as_ref()) else {
                continue;
            };
            if let Some(cell) = self.observed_key_cell(&key) {
                cell.set(live.clone());
            }
            changes.push(match (told, live) {
                (None, Some(value)) => MapDiff::Insert { key, value },
                (Some(old_value), Some(new_value)) => MapDiff::Update {
                    key,
                    old_value,
                    new_value,
                },
                (Some(old_value), None) => MapDiff::Remove { key, old_value },
                (None, None) => continue,
            });
        }

        let diff = match shape {
            Shape::Initial => Some(MapDiff::Initial {
                entries: self.snapshot(),
            }),
            Shape::Batch if !changes.is_empty() => Some(MapDiff::Batch { changes }),
            Shape::Batch | Shape::One => changes.pop(),
        };
        if let Some(diff) = diff {
            self.inner.diffs_cell.set(diff);
        }
        if let Some(len) = InFlight::tell_len(&flight) {
            self.inner.len_cell.set(len);
        }
        drop(flight);
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
