use std::{
    hash::Hash,
    marker::PhantomData,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicIsize, AtomicUsize, Ordering},
    },
};

use dashmap::DashMap;
use parking_lot::Mutex;

use super::{CellMap, CellMapInner, MapDiff, write_hook::Stored};
use crate::{
    cell::{Cell, CellImmutable, CellMutable, EffectAt},
    scheduler::order::{Sequenced, Staging, Touched, WriteOrder, key_hash},
    subscription::SubscriptionGuard,
    traits::{CellValue, Mutable},
};

/// A per-key observer and the value a write gives it.
type Point<V> = (Cell<Option<V>, CellMutable>, Option<V>);

/// What one write tells the map's cells once its rows are stored.
struct Emission<K, V>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    points: Vec<Point<V>>,
    diff: Option<MapDiff<K, V>>,
    /// Rows added less rows removed.
    grown: isize,
    sequence: Sequence,
}

/// Rows a write took out of the map, kept until the write has released it.
/// The first is held inline, so a write that displaces one row does not
/// allocate.
struct Displaced<K, V> {
    first: Option<(K, V)>,
    rest: Vec<(K, V)>,
}

impl<K, V> Default for Displaced<K, V> {
    fn default() -> Self {
        Self {
            first: None,
            rest: Vec::new(),
        }
    }
}

impl<K, V> Displaced<K, V> {
    fn extend(&mut self, row: Option<(K, V)>) {
        match row {
            Some(row) if self.first.is_none() => self.first = Some(row),
            Some(row) => self.rest.push(row),
            None => {}
        }
    }
}

/// The order in which a write tells the per-key observers, the diff
/// subscribers and the length.
#[derive(Clone, Copy)]
enum Sequence {
    PointsDiffLen,
    DiffLenPoints,
    PointsLenDiff,
}

impl<K, V> Emission<K, V>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    /// [`CellMap::store`], counting a new row.
    fn store(&mut self, map: &CellMap<K, V, CellMutable>, key: K, value: V) -> Stored<V> {
        let stored = map.store(key, value);
        if matches!(stored, Stored::Inserted) {
            self.grown = self.grown.saturating_add(1);
        }
        stored
    }

    /// [`CellMap::delete`], counting a removed row.
    fn delete(&mut self, map: &CellMap<K, V, CellMutable>, key: &K) -> Option<(K, V)> {
        let removed = map.delete(key);
        if removed.is_some() {
            self.grown = self.grown.saturating_sub(1);
        }
        removed
    }

    /// Tell `key`'s observer, if it has one, that the key now holds `value`.
    fn point(&mut self, map: &CellMap<K, V, CellMutable>, key: &K, value: Option<V>) {
        if let Some(cell) = map.observed_key_cell(key) {
            self.points.push((cell, value));
        }
    }
}

/// Adjacent diffs one batch made to a map, delivered as one.
pub(super) struct StagedDiff<K, V> {
    /// Stamp of the first row, which names the delivery.
    first: u64,
    /// Stamp of the last row.
    last: u64,
    /// The first diff, while it is the only one.
    diff: Option<MapDiff<K, V>>,
    /// Every row once there is more than one diff.
    rows: Vec<MapDiff<K, V>>,
    grown: isize,
}

impl<K, V> StagedDiff<K, V> {
    fn push(&mut self, diff: MapDiff<K, V>, grown: isize, stamp: u64) {
        fn extend<K, V>(rows: &mut Vec<MapDiff<K, V>>, diff: MapDiff<K, V>) {
            match diff {
                MapDiff::Batch { changes } => rows.extend(changes),
                row => rows.push(row),
            }
        }
        if let Some(first) = self.diff.take() {
            extend(&mut self.rows, first);
        }
        extend(&mut self.rows, diff);
        self.grown = self.grown.saturating_add(grown);
        self.last = stamp;
    }

    fn into_diff(self) -> Option<MapDiff<K, V>> {
        self.diff
            .or_else(|| (!self.rows.is_empty()).then_some(MapDiff::Batch { changes: self.rows }))
    }
}

/// The keys `diffs` name, or every key when one of them replaces the map.
fn touched_by<K: Hash, V>(diffs: &[MapDiff<K, V>]) -> Touched {
    fn collect<K: Hash, V>(diffs: &[MapDiff<K, V>], keys: &mut Vec<u64>) -> bool {
        diffs.iter().all(|diff| match diff {
            MapDiff::Initial { .. } => false,
            MapDiff::Insert { key, .. }
            | MapDiff::Remove { key, .. }
            | MapDiff::Update { key, .. } => {
                keys.push(key_hash(key));
                true
            }
            MapDiff::Batch { changes } => collect(changes, keys),
        })
    }
    if let [
        MapDiff::Insert { key, .. } | MapDiff::Remove { key, .. } | MapDiff::Update { key, .. },
    ] = diffs
    {
        return Touched::Key(key_hash(key));
    }
    let mut keys = Vec::with_capacity(diffs.len());
    if collect(diffs, &mut keys) {
        Touched::Keys(keys)
    } else {
        Touched::All
    }
}

impl<K, V> CellMapInner<K, V>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    /// A delivered write added `grown` rows. The length cell counts
    /// delivered rows, so it never includes a row whose diff its
    /// subscribers have not been sent.
    fn grew(&self, grown: isize) {
        self.len.publish(
            || {
                let len = self
                    .delivered_len
                    .fetch_add(grown, Ordering::Relaxed)
                    .saturating_add(grown);
                Some(usize::try_from(len).unwrap_or(0))
            },
            |len| self.len_cell.set(len),
        );
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
        let diffs_cell = diffs_cell.no_coalesce();
        let len_cell = Cell::new(0);

        // Mark len_cell as owned by diffs_cell so it doesn't appear as an orphan root

        Self {
            inner: Arc::new(CellMapInner {
                data: DashMap::with_shard_amount(super::SHARDS),
                key_cells: OnceLock::new(),
                key_cell_slots: AtomicUsize::new(0),
                prune_ops: AtomicUsize::new(0),
                write_hooks: super::write_hook::WriteHooks::new(),
                diffs_cell,
                len_cell,
                delivered_len: AtomicIsize::new(0),
                len: Sequenced::default(),
                order: WriteOrder::new(),
                staged: Mutex::new(Vec::new()),
                owned: Mutex::new(Vec::new()),
                name: Mutex::new(None),
            }),
            _marker: PhantomData,
        }
    }

    /// Own a subscription guard, keeping it alive as long as this `CellMap` exists.
    pub fn own(&self, guard: SubscriptionGuard) {
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

    /// Reject a write made from inside another scheduling domain's
    /// propagation before it changes the map. Every mutating method calls
    /// this first: the diff cell would reject the write too, but only after
    /// the data had changed.
    #[inline]
    fn assert_writable(&self) {
        crate::scheduler::domain::assert_write(self.inner.diffs_cell.inner.domain.as_ref());
    }

    /// Run one write. `store` changes the rows and records in the
    /// [`Emission`] what the map's cells must be told. The rows are stored in
    /// the map's write order and the emission is delivered in that order (see
    /// [`WriteOrder`]), whichever threads and batches the writes come from.
    fn write<R>(
        &self,
        touched: Touched,
        sequence: Sequence,
        store: impl FnOnce(&mut Emission<K, V>) -> R,
    ) -> R {
        self.assert_writable();
        self.maybe_prune_key_cells();
        self.inner.order.write(
            self.inner.diffs_cell.inner.domain.as_ref(),
            touched,
            || {
                let mut emission = Emission {
                    points: Vec::new(),
                    diff: None,
                    grown: 0,
                    sequence,
                };
                let result = store(&mut emission);
                (result, emission.diff.is_some().then_some(emission))
            },
            // A removal that empties the map says so in one diff.
            |emission| matches!(emission.diff, Some(MapDiff::Initial { .. })),
            |emission, staging| self.emit(emission, staging),
            || {
                let map = self.clone();
                Box::new(move |emission| map.emit(emission, Staging::Unbatched))
            },
        )
    }

    fn emit(&self, emission: Emission<K, V>, staging: Staging) {
        let Emission {
            points,
            diff,
            grown,
            sequence,
        } = emission;
        let set_points = |points: Vec<Point<V>>| {
            for (cell, value) in points {
                cell.set(value);
            }
        };
        let (first, last) = match sequence {
            Sequence::DiffLenPoints => (Vec::new(), points),
            Sequence::PointsDiffLen | Sequence::PointsLenDiff => (points, Vec::new()),
        };
        set_points(first);
        let at = match sequence {
            Sequence::PointsLenDiff => EffectAt::BeforeFanout,
            Sequence::PointsDiffLen | Sequence::DiffLenPoints => EffectAt::AfterFanout,
        };
        if let Some(diff) = diff {
            if let Staging::Batched { previous, stamp } = staging
                && !matches!(diff, MapDiff::Initial { .. })
            {
                self.stage_diff(diff, grown, at, previous, stamp);
            } else if grown == 0 {
                self.inner.diffs_cell.set(diff);
            } else {
                let map = Arc::downgrade(&self.inner);
                self.inner.diffs_cell.set_with(diff, at, move || {
                    if let Some(map) = map.upgrade() {
                        map.grew(grown);
                    }
                });
            }
        }
        set_points(last);
    }

    /// Stage a diff made inside a batch. A diff whose row directly follows
    /// one this batch staged here, and names none of the keys that delivery
    /// already carries, joins it, so the subscribers of one batch's adjacent
    /// rows to distinct keys are called once, with a `Batch` of the rows in
    /// the order they were stored. A merged `Batch` never repeats a key
    /// that no single write's diff repeated: every write to a key is its own
    /// emission, as consumers that fold a `Batch` to each key's latest row
    /// would otherwise drop the earlier ones.
    fn stage_diff(
        &self,
        diff: MapDiff<K, V>,
        grown: isize,
        at: EffectAt,
        previous: u64,
        stamp: u64,
    ) {
        let mut staged = self.inner.staged.lock();
        if previous != 0
            && let Some(open) = staged.iter_mut().find(|open| open.last == previous)
        {
            open.push(diff, grown, stamp);
            return;
        }
        staged.push(StagedDiff {
            first: stamp,
            last: stamp,
            diff: Some(diff),
            rows: Vec::new(),
            grown,
        });
        drop(staged);
        let map = Arc::clone(&self.inner);
        self.inner.diffs_cell.set_from(at, move || {
            let open = {
                let mut staged = map.staged.lock();
                let index = staged.iter().position(|open| open.first == stamp)?;
                staged.swap_remove(index)
            };
            let grown = open.grown;
            let diff = open.into_diff()?;
            Some((diff, move || {
                if grown != 0 {
                    map.grew(grown);
                }
            }))
        });
    }

    /// Insert a key-value pair, returning the old value if present.
    pub fn insert(&self, key: K, value: V) -> Option<V> {
        let touched = Touched::Key(key_hash(&key));
        self.write(touched, Sequence::PointsDiffLen, |emission| {
            let stored = emission.store(self, key.clone(), value.clone());

            // No-op update: same key/value should not emit a diff or notify observers.
            let unchanged = stored.unchanged(&value);
            let old = stored.into_old();
            if unchanged {
                return old;
            }

            emission.point(self, &key, Some(value.clone()));
            emission.diff = Some(
                old.as_ref()
                    .map(|old_value| MapDiff::Update {
                        key: key.clone(),
                        old_value: old_value.clone(),
                        new_value: value.clone(),
                    })
                    .unwrap_or(MapDiff::Insert { key, value }),
            );
            old
        })
    }

    /// Insert multiple key-value pairs and emit a single batch diff.
    pub fn insert_many(&self, entries: Vec<(K, V)>) {
        if entries.is_empty() {
            self.assert_writable();
            return;
        }
        let touched = Touched::Keys(entries.iter().map(|(key, _)| key_hash(key)).collect());
        // The replaced values drop once the write has released the map.
        let replaced = self.write(touched, Sequence::PointsDiffLen, |emission| {
            let mut changes = Vec::with_capacity(entries.len());
            let mut replaced = Vec::new();
            for (key, value) in entries {
                let stored = emission.store(self, key.clone(), value.clone());
                if stored.unchanged(&value) {
                    replaced.extend(stored.into_old());
                    continue;
                }
                let old = stored.into_old();
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
                emission.point(self, &key, Some(value));
            }
            if !changes.is_empty() {
                emission.diff = Some(MapDiff::Batch { changes });
            }
            replaced
        });
        drop(replaced);
    }

    /// Remove a key, returning the old value if present.
    pub fn remove(&self, key: &K) -> Option<V> {
        let touched = Touched::Key(key_hash(key));
        self.write(touched, Sequence::DiffLenPoints, |emission| {
            let (key, old_value) = emission.delete(self, key)?;
            emission.point(self, &key, None);
            emission.diff = Some(MapDiff::Remove {
                key,
                old_value: old_value.clone(),
            });
            Some(old_value)
        })
    }

    /// Remove multiple keys and emit a single batch diff.
    pub fn remove_many(&self, keys: Vec<K>) {
        if keys.is_empty() {
            self.assert_writable();
            return;
        }
        let touched = Touched::Keys(keys.iter().map(key_hash).collect());
        self.write(touched, Sequence::PointsDiffLen, |emission| {
            let original_len = self.inner.data.len();
            let mut changes = Vec::new();
            for key in keys {
                if let Some((key, old_value)) = emission.delete(self, &key) {
                    emission.point(self, &key, None);
                    changes.push(MapDiff::Remove { key, old_value });
                }
            }
            if changes.is_empty() {
                return;
            }
            let emptied = self.inner.data.is_empty() && changes.len() == original_len;
            emission.diff = Some(if emptied {
                MapDiff::Initial {
                    entries: Vec::new(),
                }
            } else {
                MapDiff::Batch { changes }
            });
        });
    }

    /// Replace all entries atomically, emitting a single `Batch` diff.
    ///
    /// Removes keys not in `entries`, inserts/updates keys that are.
    /// Skips no-op updates (existing key with equal value).
    /// Emits `MapDiff::Batch` with the actual changes so downstream
    /// subscribers see one atomic replacement instead of N individual diffs.
    pub fn replace_all(&self, entries: Vec<(K, V)>) {
        let replaced = self.write(Touched::All, Sequence::PointsDiffLen, |emission| {
            let new_keys: std::collections::HashSet<&K> = entries.iter().map(|(k, _)| k).collect();
            let mut changes = Vec::new();
            let mut replaced = Vec::new();

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
                if let Some((key, old_value)) = emission.delete(self, key) {
                    emission.point(self, &key, None);
                    changes.push(MapDiff::Remove { key, old_value });
                }
            }

            // Insert/update entries
            for (key, value) in entries {
                let stored = emission.store(self, key.clone(), value.clone());
                if stored.unchanged(&value) {
                    replaced.extend(stored.into_old());
                    continue;
                }
                let old = stored.into_old();
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
                emission.point(self, &key, Some(value));
            }

            if !changes.is_empty() {
                emission.diff = Some(MapDiff::Batch { changes });
            }
            replaced
        });
        drop(replaced);
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
            return self.apply_batch(changes);
        }
        // What the write displaced drops once it has released the map.
        let touched = touched_by(std::slice::from_ref(&diff));
        let displaced = self.write(touched, Sequence::PointsLenDiff, |emission| {
            let mut displaced = Displaced::default();
            if self.apply(&diff, emission, &mut displaced) {
                emission.diff = Some(diff);
            }
            displaced
        });
        drop(displaced);
    }

    /// Store what one diff other than a `Batch` describes, moving the rows it
    /// displaces into `displaced`. Returns `false` when the diff changes
    /// nothing and is dropped.
    fn apply(
        &self,
        diff: &MapDiff<K, V>,
        emission: &mut Emission<K, V>,
        displaced: &mut Displaced<K, V>,
    ) -> bool {
        match diff {
            MapDiff::Initial { entries } => {
                let stale_keys: Vec<K> = self.collect(|key, _| key.clone());
                for key in stale_keys {
                    displaced.extend(emission.delete(self, &key));
                    emission.point(self, &key, None);
                }
                for (key, value) in entries {
                    drop(emission.store(self, key.clone(), value.clone()));
                    emission.point(self, key, Some(value.clone()));
                }
            }
            MapDiff::Insert { key, value } => {
                displaced.extend(
                    emission
                        .store(self, key.clone(), value.clone())
                        .into_old()
                        .map(|old| (key.clone(), old)),
                );
                emission.point(self, key, Some(value.clone()));
            }
            MapDiff::Remove { key, .. } => {
                displaced.extend(emission.delete(self, key));
                emission.point(self, key, None);
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
                displaced.extend(
                    emission
                        .store(self, key.clone(), new_value.clone())
                        .into_old()
                        .map(|old| (key.clone(), old)),
                );
                emission.point(self, key, Some(new_value.clone()));
            }
            MapDiff::Batch { .. } => return false,
        }
        true
    }

    /// Apply a batch of diffs and emit them as one `MapDiff::Batch`.
    pub fn apply_batch(&self, changes: Vec<MapDiff<K, V>>) {
        fn apply_one<K, V>(
            map: &CellMap<K, V, CellMutable>,
            diff: MapDiff<K, V>,
            emission: &mut Emission<K, V>,
            displaced: &mut Displaced<K, V>,
        ) -> Option<MapDiff<K, V>>
        where
            K: Hash + Eq + CellValue,
            V: CellValue,
        {
            if let MapDiff::Batch { changes } = diff {
                let applied: Vec<_> = changes
                    .into_iter()
                    .filter_map(|change| apply_one(map, change, emission, displaced))
                    .collect();
                return (!applied.is_empty()).then_some(MapDiff::Batch { changes: applied });
            }
            map.apply(&diff, emission, displaced).then_some(diff)
        }

        if changes.is_empty() {
            self.assert_writable();
            return;
        }
        let touched = touched_by(&changes);
        let displaced = self.write(touched, Sequence::PointsLenDiff, |emission| {
            let mut displaced = Displaced::default();
            let applied: Vec<_> = changes
                .into_iter()
                .filter_map(|change| apply_one(self, change, emission, &mut displaced))
                .collect();
            if !applied.is_empty() {
                emission.diff = Some(MapDiff::Batch { changes: applied });
            }
            displaced
        });
        drop(displaced);
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
