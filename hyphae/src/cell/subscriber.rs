#![allow(clippy::redundant_pub_crate)]

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use parking_lot::Mutex;
use rustc_hash::FxHashMap;
use uuid::Uuid;

use crate::signal::Signal;

/// Indexed subscriber registry: O(1) subscribe/unsubscribe by subscription id,
/// with a lazily-rebuilt [`SubSnapshot`] for lock-free notify iteration.
///
/// `index` is authoritative. `snapshot` is a cached view of it that `notify`
/// clones (an `Arc` bump, or nothing for the 0/1-subscriber cases) and iterates
/// *without* the lock held. Mutations touch
/// only `index` and set `dirty`; they never rebuild the snapshot, so subscribe
/// and unsubscribe are O(1) instead of the old copy-on-write O(n) `Vec` rebuild
/// (the `eq<Uuid>` linear scan that dominated the profile). The snapshot is
/// rebuilt once, lazily, on the next `notify` after any mutation — amortizing
/// the O(n) rebuild across every change since the previous notify.
///
/// Displaced `Arc`s (a removed subscriber, or the replaced snapshot) are
/// *returned* from the mutating methods rather than dropped inline: the caller
/// must drop them **after** releasing the mutex, because a subscriber's drop
/// can cascade into upstream `CellInner` drops that acquire other cell mutexes,
/// and running that under our lock can deadlock two concurrently-dropping cells.
/// A notify snapshot, sized to the subscriber count so the common cases don't
/// pay for the general one. Profiling rship's HRLV playback showed the *vast
/// majority* of cells carry exactly one subscriber, yet churn-heavy sources
/// (`switch_map` rewiring its input every fire) re-`dirty` the registry each
/// notify — so the old always-`Arc<Vec>` snapshot heap-allocated a `Vec` *and*
/// an `Arc` per fire just to hold a single element.
///
/// - `Zero` — no subscribers; an empty slice, no allocation.
/// - `One` — the single subscriber inline; cloning is one `Arc` bump, no heap.
/// - `Many` — today's path: an `Arc<Vec>` cloned by ref-count bump.
///
/// [`as_slice`](SubSnapshot::as_slice) unifies the three for the consumer
/// (sequential fanout and `par_for_each` alike): `One` yields a length-1 slice
/// via [`std::slice::from_ref`] over its inline tuple, so no variant needs a
/// backing `Vec`.
pub(crate) enum SubSnapshot<S> {
    Zero,
    One((Uuid, Arc<S>)),
    Many(Arc<Vec<(Uuid, Arc<S>)>>),
}

// Manual `Clone` (not derived) so the bound is `Arc<S>: Clone` — always true —
// rather than `S: Clone`, which the subscriber payloads don't satisfy.
impl<S> Clone for SubSnapshot<S> {
    fn clone(&self) -> Self {
        match self {
            Self::Zero => Self::Zero,
            Self::One(pair) => Self::One(pair.clone()),
            Self::Many(subs) => Self::Many(subs.clone()),
        }
    }
}

impl<S> SubSnapshot<S> {
    /// View the snapshot as a slice for iteration — the same shape for all three
    /// variants, so callers fan out identically whether there are zero, one, or
    /// many subscribers. Borrows from `self`; the caller keeps `self` alive (and
    /// drops it outside the lock) for the duration of the fanout.
    pub(crate) fn as_slice(&self) -> &[(Uuid, Arc<S>)] {
        match self {
            Self::Zero => &[],
            Self::One(pair) => std::slice::from_ref(pair),
            Self::Many(subs) => subs.as_slice(),
        }
    }
}

/// The authoritative subscriber store, sized to the subscriber count so the
/// 0/1-subscriber majority never allocates a hash table. Most cells in a large
/// reactive graph carry at most one subscriber for their whole life (a `map`
/// feeding one downstream, a leaf sink); for those, `FxHashMap`'s first-insert
/// bucket allocation was pure per-cell overhead — paid once per cell, but
/// across millions of cells.
///
/// - `Zero` / `One` — inline, no heap.
/// - `Many` — the `FxHashMap` path, entered on the 1 → 2 transition.
///
/// **No demotion.** Once a registry reaches `Many` it stays there even if it
/// shrinks back to one subscriber. Demoting would thrash the hash table's
/// allocation for cells that oscillate across the 1/2 boundary (`switch_map`
/// re-knitting subscribe-before-unsubscribe transiently holds two); keeping the
/// map matches the previous always-`FxHashMap` behaviour for exactly those
/// cells, while cells that never exceed one subscriber pay nothing. All
/// operations stay O(1); iteration order is unspecified (it always was).
enum SubIndex<S> {
    Zero,
    One(Uuid, Arc<S>),
    Many(FxHashMap<Uuid, Arc<S>>),
}

impl<S> SubIndex<S> {
    fn len(&self) -> usize {
        match self {
            Self::Zero => 0,
            Self::One(..) => 1,
            Self::Many(map) => map.len(),
        }
    }

    /// Insert a subscriber, returning any Arc displaced by a same-id overwrite
    /// (normally `None`, since ids are fresh) for the caller to drop outside the
    /// lock.
    fn insert(&mut self, id: Uuid, sub: Arc<S>) -> Option<Arc<S>> {
        match self {
            Self::Zero => {
                *self = Self::One(id, sub);
                return None;
            }
            Self::One(existing_id, existing_sub) => {
                if *existing_id == id {
                    return Some(std::mem::replace(existing_sub, sub));
                }
                // Different id: fall out of the match to promote (the borrow of
                // `existing_sub` must end before we reassign `*self`).
            }
            Self::Many(map) => {
                return map.insert(id, sub);
            }
        }

        // Reached only from `One` with a different id: promote to `Many`,
        // carrying the existing single subscriber plus the new one.
        let previous = std::mem::replace(self, Self::Zero);
        let Self::One(old_id, old_sub) = previous else {
            *self = previous;
            return None;
        };
        let mut map = FxHashMap::default();
        map.insert(old_id, old_sub);
        map.insert(id, sub);
        *self = Self::Many(map);
        None
    }

    /// Remove a subscriber by id, returning the removed Arc (if present) for the
    /// caller to drop outside the lock. Never demotes `Many` (see the type doc).
    fn remove(&mut self, id: &Uuid) -> Option<Arc<S>> {
        match self {
            Self::Zero => None,
            Self::One(existing_id, _) => {
                if *existing_id != *id {
                    return None;
                }
                let previous = std::mem::replace(self, Self::Zero);
                if let Self::One(_, sub) = previous {
                    Some(sub)
                } else {
                    *self = previous;
                    None
                }
            }
            Self::Many(map) => map.remove(id),
        }
    }
}

pub(crate) struct SubscriberRegistry<S> {
    index: SubIndex<S>,
    snapshot: SubSnapshot<S>,
    dirty: bool,
}

impl<S> SubscriberRegistry<S> {
    pub(super) const fn new() -> Self {
        Self {
            index: SubIndex::Zero,
            snapshot: SubSnapshot::Zero,
            dirty: false,
        }
    }

    /// Insert a subscriber. O(1). Returns any Arc it displaced (normally `None`,
    /// since ids are fresh) for the caller to drop outside the lock.
    #[must_use = "displaced subscriber must be dropped outside the lock"]
    pub(super) fn insert(&mut self, id: Uuid, sub: Arc<S>) -> Option<Arc<S>> {
        self.dirty = true;
        self.index.insert(id, sub)
    }

    /// Remove a subscriber by id. O(1). Returns `(removed_arc, stale_snapshot)`;
    /// the caller must drop **both** outside the lock (each may hold the last
    /// ref to an unsubscribed subscriber whose drop cascades into upstream cell
    /// drops that take other cell mutexes).
    ///
    /// The stale-snapshot release is load-bearing, not bookkeeping: `remove`
    /// only marks the registry `dirty`, and the cached `snapshot` is otherwise
    /// rebuilt lazily on the *next* notify. A cell that is never notified again
    /// — e.g. a superseded `switch_map` inner's `CellMap` `diffs_cell` — would
    /// then keep the just-removed subscriber pinned in `snapshot` forever, and
    /// a subscriber whose closure holds a `map_keepalive` `Arc<CellMapInner>`
    /// (`CellMap::items`/`subscribe_diffs`) leaks the entire map. Clearing the
    /// snapshot here costs no extra rebuild — `dirty` already forces the next
    /// notify to rebuild from `index` — it just stops the idle pin.
    #[must_use = "removed subscriber and stale snapshot must be dropped outside the lock"]
    pub(super) fn remove(&mut self, id: &Uuid) -> (Option<Arc<S>>, Option<SubSnapshot<S>>) {
        let removed = self.index.remove(id);
        if removed.is_some() {
            self.dirty = true;
            let stale = std::mem::replace(&mut self.snapshot, SubSnapshot::Zero);
            (removed, Some(stale))
        } else {
            (removed, None)
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.index.len()
    }

    /// Current notify snapshot, rebuilt from `index` if the index changed since
    /// the last call. Returns `(snapshot_to_iterate, displaced_old_snapshot)`;
    /// the caller must drop the displaced snapshot outside the lock — it may
    /// hold the last ref to an unsubscribed subscriber whose drop cascades.
    #[must_use = "displaced snapshot must be dropped outside the lock"]
    pub(crate) fn snapshot(&mut self) -> (SubSnapshot<S>, Option<SubSnapshot<S>>) {
        if self.dirty {
            // Size the rebuilt snapshot to the subscriber count, mirroring the
            // index's own shape: the 0/1 cases (1 being the overwhelming
            // majority) avoid the `Vec` + `Arc` heap allocation the general
            // path pays.
            let next = match &self.index {
                SubIndex::Zero => SubSnapshot::Zero,
                SubIndex::One(id, sub) => SubSnapshot::One((*id, sub.clone())),
                SubIndex::Many(map) => SubSnapshot::Many(Arc::new(
                    map.iter().map(|(id, sub)| (*id, sub.clone())).collect(),
                )),
            };
            let old = std::mem::replace(&mut self.snapshot, next);
            self.dirty = false;
            (self.snapshot.clone(), Some(old))
        } else {
            (self.snapshot.clone(), None)
        }
    }
}

/// Type alias for subscriber callback functions.
pub(crate) type SubscriberCallback<T> = Arc<dyn Fn(&Signal<T>) + Send + Sync>;

pub(crate) struct Subscriber<T> {
    pub(crate) callback: SubscriberCallback<T>,
    initializing: AtomicBool,
    pending: Mutex<Vec<Signal<T>>>,
}

impl<T: Clone> Subscriber<T> {
    pub(crate) fn new(callback: impl Fn(&Signal<T>) + Send + Sync + 'static) -> Self {
        Self {
            callback: Arc::new(callback),
            initializing: AtomicBool::new(true),
            pending: Mutex::new(Vec::new()),
        }
    }

    pub(crate) fn new_live(callback: SubscriberCallback<T>) -> Self {
        Self {
            callback,
            initializing: AtomicBool::new(false),
            pending: Mutex::new(Vec::new()),
        }
    }

    /// Queue notifications until the synchronous current-value replay has
    /// completed. This prevents a concurrent notify from running before that
    /// replay and then being overwritten by an older value delivered last.
    pub(super) fn deliver(&self, signal: &Signal<T>) {
        if self.initializing.load(Ordering::Acquire) {
            let mut pending = self.pending.lock();
            if self.initializing.load(Ordering::Relaxed) {
                pending.push(signal.clone());
                return;
            }
            drop(pending);
        }
        (self.callback)(signal);
    }

    pub(super) fn finish_initial(&self, initial: &Signal<T>) {
        (self.callback)(initial);
        loop {
            let pending = {
                let mut pending = self.pending.lock();
                if pending.is_empty() {
                    self.initializing.store(false, Ordering::Release);
                    return;
                }
                std::mem::take(&mut *pending)
            };
            for signal in pending {
                (self.callback)(&signal);
            }
        }
    }
}

/// Type alias for fallible subscriber callbacks. See [`WatchableResult::subscribe_result`].
pub(crate) type ResultSubscriberCallback<T> =
    Arc<dyn Fn(&Signal<T>) -> Result<(), String> + Send + Sync>;

pub(crate) struct ResultSubscriber<T> {
    pub(crate) callback: ResultSubscriberCallback<T>,
    initializing: AtomicBool,
    pending: Mutex<Vec<Signal<T>>>,
}

impl<T: Clone> ResultSubscriber<T> {
    pub(crate) fn new(
        callback: impl Fn(&Signal<T>) -> Result<(), String> + Send + Sync + 'static,
    ) -> Self {
        Self {
            callback: Arc::new(callback),
            initializing: AtomicBool::new(true),
            pending: Mutex::new(Vec::new()),
        }
    }

    pub(super) fn deliver(&self, signal: &Signal<T>) -> Result<(), String> {
        if self.initializing.load(Ordering::Acquire) {
            let mut pending = self.pending.lock();
            if self.initializing.load(Ordering::Relaxed) {
                pending.push(signal.clone());
                return Ok(());
            }
            drop(pending);
        }
        (self.callback)(signal)
    }

    pub(super) fn finish_initial(&self, initial: &Signal<T>, on_error: impl Fn(&str)) {
        if let Err(error) = (self.callback)(initial) {
            on_error(&error);
        }
        loop {
            let pending = {
                let mut pending = self.pending.lock();
                if pending.is_empty() {
                    self.initializing.store(false, Ordering::Release);
                    return;
                }
                std::mem::take(&mut *pending)
            };
            for signal in pending {
                if let Err(error) = (self.callback)(&signal) {
                    on_error(&error);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use uuid::Uuid;

    use super::{SubIndex, SubSnapshot};

    // The `Arc<S>` payload stands in for a real subscriber; only identity and
    // ref-count matter here, so `i32` is enough.
    fn sub(v: i32) -> Arc<i32> {
        Arc::new(v)
    }

    #[test]
    fn zero_and_one_stay_inline() {
        let mut idx: SubIndex<i32> = SubIndex::Zero;
        assert!(matches!(idx, SubIndex::Zero));
        assert_eq!(idx.len(), 0);

        // First insert → One, no hash table.
        assert!(idx.insert(Uuid::new_v4(), sub(1)).is_none());
        assert!(matches!(idx, SubIndex::One(..)));
        assert_eq!(idx.len(), 1);
    }

    #[test]
    fn same_id_insert_overwrites_and_returns_old() {
        let id = Uuid::new_v4();
        let mut idx: SubIndex<i32> = SubIndex::Zero;
        let first = sub(1);
        assert!(idx.insert(id, first.clone()).is_none());

        // Re-inserting the same id swaps the Arc and returns the displaced one,
        // without promoting to Many.
        let displaced = idx.insert(id, sub(2));
        assert!(displaced.is_some(), "old sub returned");
        let Some(displaced) = displaced else { return };
        assert!(Arc::ptr_eq(&displaced, &first));
        assert!(matches!(idx, SubIndex::One(..)));
        assert_eq!(idx.len(), 1);
    }

    #[test]
    fn second_distinct_id_promotes_to_many_keeping_both() {
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        let mut idx: SubIndex<i32> = SubIndex::Zero;
        assert!(idx.insert(a, sub(1)).is_none());
        // 1 → 2 promotes; no displacement.
        assert!(idx.insert(b, sub(2)).is_none());
        assert!(matches!(idx, SubIndex::Many(_)));
        assert_eq!(idx.len(), 2);

        // Both survive the promotion.
        let snap = build_snapshot(&idx);
        let ids: Vec<Uuid> = snap.as_slice().iter().map(|(id, _)| *id).collect();
        assert!(ids.contains(&a) && ids.contains(&b));
    }

    #[test]
    fn remove_from_one_returns_to_zero() {
        let id = Uuid::new_v4();
        let mut idx: SubIndex<i32> = SubIndex::Zero;
        let s = sub(7);
        let _ = idx.insert(id, s.clone());

        let removed = idx.remove(&id);
        assert!(removed.is_some(), "present");
        let Some(removed) = removed else { return };
        assert!(Arc::ptr_eq(&removed, &s));
        assert!(matches!(idx, SubIndex::Zero));
        assert_eq!(idx.len(), 0);

        // Removing a missing id from Zero is a no-op.
        assert!(idx.remove(&Uuid::new_v4()).is_none());
    }

    #[test]
    fn remove_wrong_id_from_one_is_noop() {
        let mut idx: SubIndex<i32> = SubIndex::Zero;
        let _ = idx.insert(Uuid::new_v4(), sub(1));
        assert!(idx.remove(&Uuid::new_v4()).is_none());
        assert!(matches!(idx, SubIndex::One(..)));
        assert_eq!(idx.len(), 1);
    }

    #[test]
    fn many_does_not_demote_when_shrinking() {
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        let mut idx: SubIndex<i32> = SubIndex::Zero;
        let _ = idx.insert(a, sub(1));
        let _ = idx.insert(b, sub(2));
        assert!(matches!(idx, SubIndex::Many(_)));

        // Shrinking back to one subscriber keeps the hash table (no demotion),
        // so cells oscillating across the 1/2 boundary don't thrash the alloc.
        let _ = idx.remove(&a);
        assert_eq!(idx.len(), 1);
        assert!(matches!(idx, SubIndex::Many(_)));
    }

    // Mirror `SubscriberRegistry::snapshot`'s index→snapshot mapping so tests can
    // read the contents back out without a full registry.
    fn build_snapshot(idx: &SubIndex<i32>) -> SubSnapshot<i32> {
        match idx {
            SubIndex::Zero => SubSnapshot::Zero,
            SubIndex::One(id, s) => SubSnapshot::One((*id, s.clone())),
            SubIndex::Many(map) => SubSnapshot::Many(Arc::new(
                map.iter().map(|(id, s)| (*id, s.clone())).collect(),
            )),
        }
    }
}
