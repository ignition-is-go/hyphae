#[cfg(feature = "metrics")]
use std::time::Duration;
use std::{
    fmt::Debug,
    marker::PhantomData,
    panic::Location,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, Ordering},
    },
};

#[cfg(feature = "metrics")]
use arc_swap::ArcSwap;
use dashmap::DashMap;
use rustc_hash::FxHashMap;
use uuid::Uuid;

#[cfg(feature = "metrics")]
use crate::metrics::CellMetrics;
use crate::{
    signal::Signal,
    subscription::SubscriptionGuard,
    traits::{CellValue, DepNode, Gettable, Mutable, Watchable, WatchableResult},
};

/// Information about a slow subscriber callback.
#[cfg(feature = "metrics")]
#[derive(Debug, Clone)]
pub struct SlowSubscriberAlert {
    /// The subscriber ID.
    pub subscriber_id: Uuid,
    /// How long the subscriber took (nanoseconds).
    pub duration_ns: u64,
    /// The configured threshold (nanoseconds).
    pub threshold_ns: u64,
}

#[cfg(feature = "metrics")]
type SlowSubscriberCallback = Arc<dyn Fn(SlowSubscriberAlert) + Send + Sync>;

#[derive(Debug, Clone)]
pub struct CellMutable;

#[derive(Debug, Clone)]
pub struct CellImmutable;

/// The inner data of a Cell, wrapped in Arc for shared ownership.
pub(crate) struct CellInner<T> {
    pub(crate) id: Uuid,
    /// Infallible subscriber registry. See [`SubscriberRegistry`]: an id-keyed
    /// index (O(1) subscribe/unsubscribe) fronting a lazily-rebuilt `Arc<Vec>`
    /// snapshot that `notify` clones and iterates lock-free, so user callbacks
    /// never run with an internal cell mutex held.
    pub(crate) subscribers: parking_lot::Mutex<SubscriberRegistry<Subscriber<T>>>,
    /// Fallible subscribers. Invoked after `subscribers` on each notify;
    /// `Err` values are logged via `log::error!` and do not propagate.
    pub(crate) result_subscribers: parking_lot::Mutex<SubscriberRegistry<ResultSubscriber<T>>>,
    /// The cell's current value. Stored as `Mutex<Arc<T>>` rather than
    /// `ArcSwap<T>` so writes don't pay arc_swap's reader-debt-slot scan.
    /// Reads `lock + clone (Arc bump) + unlock`. Writes
    /// `lock + assign (drops old Arc inline) + unlock`. Old values reclaim
    /// via `Arc` refcounting — readers holding clones keep the value alive
    /// until they drop.
    pub(crate) value: Mutex<Arc<T>>,
    /// Optional human-readable name for tracing/debugging. Cold path — set
    /// rarely via `with_name`, read from `DepNode::name`. Mutex avoids the
    /// per-cell ArcSwap drop cost paid on every cell teardown.
    pub(crate) name: Mutex<Option<Arc<str>>>,
    /// Subscription guards owned by this cell (dropped when cell drops, provides dependency tracking).
    pub(crate) owned: DashMap<Uuid, SubscriptionGuard>,
    /// Whether this cell has completed (no more values will be emitted).
    pub(crate) completed: AtomicBool,
    /// Whether this cell has errored.
    pub(crate) errored: AtomicBool,
    /// The error, if any. Cold path — only written when the cell errors,
    /// read by error/subscribe paths.
    pub(crate) error: Mutex<Option<Arc<anyhow::Error>>>,
    /// Scheduler height cache: packed `(epoch << 32) | height`. The scheduler
    /// computes a cell's propagation height (`1 + max(dep.height)`) once per
    /// topology epoch and caches it here, so a steady-state batch reads height
    /// as a single atomic load instead of walking `deps()` every notify. `0`
    /// means "never computed" (epoch 0 is never current). Invalidated lazily by
    /// bumping the global topology epoch on any edge change.
    #[cfg(feature = "scheduler")]
    pub(crate) height_cache: std::sync::atomic::AtomicU64,
    /// Scheduler coalescing policy. When `true`, the scheduler enqueues every
    /// notify from this cell as a distinct height-ordered op instead of
    /// last-write-wins coalescing them — preserving the event semantics
    /// (scan/pairwise/merge, or a hand-rolled stateful `map`) that a dropped
    /// intermediate would corrupt. Stamped at birth inside a
    /// [`scheduler::no_coalesce`](crate::scheduler::no_coalesce) scope, or after
    /// the fact via [`Cell::no_coalesce`]. Default `false` (coalesce), so the
    /// behavior-cell majority gets the glitch-free win.
    #[cfg(feature = "scheduler")]
    pub(crate) no_coalesce: AtomicBool,
    /// Optional metrics for observability.
    #[cfg(feature = "metrics")]
    pub(crate) metrics: Option<Arc<CellMetrics>>,
    /// Slow subscriber threshold (nanoseconds). None = disabled.
    #[cfg(feature = "metrics")]
    pub(crate) slow_subscriber_threshold_ns: ArcSwap<Option<u64>>,
    /// Callback for slow subscriber alerts.
    #[cfg(feature = "metrics")]
    pub(crate) slow_subscriber_callback: ArcSwap<Option<SlowSubscriberCallback>>,
    /// Source location where this cell was created (via #[track_caller]).
    #[allow(dead_code)]
    pub(crate) caller: &'static Location<'static>,
}

/// A reactive cell that holds a value and notifies subscribers on change.
pub struct Cell<T, M> {
    pub(crate) inner: Arc<CellInner<T>>,
    pub(crate) _marker: PhantomData<M>,
}

/// A weak reference to a Cell that doesn't prevent it from being dropped.
pub struct WeakCell<T, M> {
    inner: Weak<CellInner<T>>,
    _marker: PhantomData<M>,
}

impl<T, M> WeakCell<T, M> {
    /// Try to upgrade to a strong Cell reference.
    /// Returns None if the Cell has been dropped.
    pub fn upgrade(&self) -> Option<Cell<T, M>> {
        self.inner.upgrade().map(|inner| Cell {
            inner,
            _marker: PhantomData,
        })
    }

    /// Whether the referenced Cell is still alive (has live strong references).
    ///
    /// Cheaper than `upgrade().is_some()` — it only reads the strong count and
    /// never materializes (or transiently reference-counts) a `Cell`, so it is
    /// safe to call in a hot sweep over many weaks.
    pub fn is_alive(&self) -> bool {
        self.inner.strong_count() > 0
    }
}

impl<T, M> Clone for WeakCell<T, M> {
    fn clone(&self) -> Self {
        WeakCell {
            inner: self.inner.clone(),
            _marker: PhantomData,
        }
    }
}

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
/// (switch_map rewiring its input every fire) re-`dirty` the registry each
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
            SubSnapshot::Zero => SubSnapshot::Zero,
            SubSnapshot::One(pair) => SubSnapshot::One(pair.clone()),
            SubSnapshot::Many(subs) => SubSnapshot::Many(subs.clone()),
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
            SubSnapshot::Zero => &[],
            SubSnapshot::One(pair) => std::slice::from_ref(pair),
            SubSnapshot::Many(subs) => subs.as_slice(),
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
/// allocation for cells that oscillate across the 1/2 boundary (switch_map
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
            SubIndex::Zero => 0,
            SubIndex::One(..) => 1,
            SubIndex::Many(map) => map.len(),
        }
    }

    /// Insert a subscriber, returning any Arc displaced by a same-id overwrite
    /// (normally `None`, since ids are fresh) for the caller to drop outside the
    /// lock.
    fn insert(&mut self, id: Uuid, sub: Arc<S>) -> Option<Arc<S>> {
        match self {
            SubIndex::Zero => {
                *self = SubIndex::One(id, sub);
                return None;
            }
            SubIndex::One(existing_id, existing_sub) => {
                if *existing_id == id {
                    return Some(std::mem::replace(existing_sub, sub));
                }
                // Different id: fall out of the match to promote (the borrow of
                // `existing_sub` must end before we reassign `*self`).
            }
            SubIndex::Many(map) => {
                return map.insert(id, sub);
            }
        }

        // Reached only from `One` with a different id: promote to `Many`,
        // carrying the existing single subscriber plus the new one.
        let (old_id, old_sub) = match std::mem::replace(self, SubIndex::Zero) {
            SubIndex::One(old_id, old_sub) => (old_id, old_sub),
            _ => unreachable!("promotion is entered only from the One arm"),
        };
        let mut map = FxHashMap::default();
        map.insert(old_id, old_sub);
        map.insert(id, sub);
        *self = SubIndex::Many(map);
        None
    }

    /// Remove a subscriber by id, returning the removed Arc (if present) for the
    /// caller to drop outside the lock. Never demotes `Many` (see the type doc).
    fn remove(&mut self, id: &Uuid) -> Option<Arc<S>> {
        match self {
            SubIndex::Zero => None,
            SubIndex::One(existing_id, _) => {
                if *existing_id != *id {
                    return None;
                }
                match std::mem::replace(self, SubIndex::Zero) {
                    SubIndex::One(_, sub) => Some(sub),
                    _ => unreachable!("just matched One"),
                }
            }
            SubIndex::Many(map) => map.remove(id),
        }
    }
}

pub(crate) struct SubscriberRegistry<S> {
    index: SubIndex<S>,
    snapshot: SubSnapshot<S>,
    dirty: bool,
}

impl<S> SubscriberRegistry<S> {
    fn new() -> Self {
        Self {
            index: SubIndex::Zero,
            snapshot: SubSnapshot::Zero,
            dirty: false,
        }
    }

    /// Insert a subscriber. O(1). Returns any Arc it displaced (normally `None`,
    /// since ids are fresh) for the caller to drop outside the lock.
    #[must_use = "displaced subscriber must be dropped outside the lock"]
    fn insert(&mut self, id: Uuid, sub: Arc<S>) -> Option<Arc<S>> {
        self.dirty = true;
        self.index.insert(id, sub)
    }

    /// Remove a subscriber by id. O(1). Returns the removed Arc (if present) for
    /// the caller to drop outside the lock.
    #[must_use = "removed subscriber must be dropped outside the lock"]
    fn remove(&mut self, id: &Uuid) -> Option<Arc<S>> {
        let removed = self.index.remove(id);
        if removed.is_some() {
            self.dirty = true;
        }
        removed
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
}

impl<T> Subscriber<T> {
    pub(crate) fn new(callback: impl Fn(&Signal<T>) + Send + Sync + 'static) -> Self {
        Self {
            callback: Arc::new(callback),
        }
    }
}

/// Type alias for fallible subscriber callbacks. See [`WatchableResult::subscribe_result`].
pub(crate) type ResultSubscriberCallback<T> =
    Arc<dyn Fn(&Signal<T>) -> Result<(), String> + Send + Sync>;

pub(crate) struct ResultSubscriber<T> {
    pub(crate) callback: ResultSubscriberCallback<T>,
}

impl<T> ResultSubscriber<T> {
    pub(crate) fn new(
        callback: impl Fn(&Signal<T>) -> Result<(), String> + Send + Sync + 'static,
    ) -> Self {
        Self {
            callback: Arc::new(callback),
        }
    }
}

impl<T: CellValue> Cell<T, CellMutable> {
    #[track_caller]
    pub fn new(initial_value: T) -> Self {
        let inner = Arc::new(CellInner {
            id: Uuid::new_v4(),
            subscribers: parking_lot::Mutex::new(SubscriberRegistry::new()),
            result_subscribers: parking_lot::Mutex::new(SubscriberRegistry::new()),
            value: Mutex::new(Arc::new(initial_value)),
            name: Mutex::new(None),
            owned: DashMap::new(),
            completed: AtomicBool::new(false),
            errored: AtomicBool::new(false),
            error: Mutex::new(None),
            #[cfg(feature = "scheduler")]
            height_cache: std::sync::atomic::AtomicU64::new(0),
            #[cfg(feature = "scheduler")]
            no_coalesce: AtomicBool::new(crate::scheduler::birth_no_coalesce()),
            #[cfg(feature = "metrics")]
            metrics: default_metrics(),
            #[cfg(feature = "metrics")]
            slow_subscriber_threshold_ns: ArcSwap::from_pointee(None),
            #[cfg(feature = "metrics")]
            slow_subscriber_callback: ArcSwap::from_pointee(None),
            caller: Location::caller(),
        });
        #[cfg(feature = "inspector")]
        crate::registry::registry().register(inner.id, Arc::downgrade(&inner) as Weak<dyn DepNode>);
        #[cfg(feature = "trace")]
        crate::tracing::register_cell(inner.id, Some(Location::caller().to_string()));
        Self {
            inner,
            _marker: PhantomData,
        }
    }

    /// Create a new mutable cell with metrics collection enabled.
    #[cfg(feature = "metrics")]
    #[track_caller]
    pub fn with_metrics(initial_value: T) -> Self {
        let inner = Arc::new(CellInner {
            id: Uuid::new_v4(),
            subscribers: parking_lot::Mutex::new(SubscriberRegistry::new()),
            result_subscribers: parking_lot::Mutex::new(SubscriberRegistry::new()),
            value: Mutex::new(Arc::new(initial_value)),
            name: Mutex::new(None),
            owned: DashMap::new(),
            completed: AtomicBool::new(false),
            errored: AtomicBool::new(false),
            error: Mutex::new(None),
            #[cfg(feature = "scheduler")]
            height_cache: std::sync::atomic::AtomicU64::new(0),
            #[cfg(feature = "scheduler")]
            no_coalesce: AtomicBool::new(crate::scheduler::birth_no_coalesce()),
            metrics: Some(Arc::new(CellMetrics::new())),
            slow_subscriber_threshold_ns: ArcSwap::from_pointee(None),
            slow_subscriber_callback: ArcSwap::from_pointee(None),
            caller: Location::caller(),
        });
        #[cfg(feature = "inspector")]
        crate::registry::registry().register(inner.id, Arc::downgrade(&inner) as Weak<dyn DepNode>);
        #[cfg(feature = "trace")]
        crate::tracing::register_cell(inner.id, Some(Location::caller().to_string()));
        Self {
            inner,
            _marker: PhantomData,
        }
    }
    /// Configure slow subscriber detection.
    ///
    /// When any subscriber callback takes longer than `threshold`, the `callback`
    /// is invoked with details about the slow subscriber.
    ///
    /// Note: This requires metrics to be enabled. If metrics are not enabled,
    /// subscriber timing is not tracked and slow subscriber detection will not work.
    ///
    /// # Example
    ///
    /// ```
    /// use hyphae::{Cell, Mutable};
    /// use std::time::Duration;
    ///
    /// let cell = Cell::with_metrics(0);
    /// cell.on_slow_subscriber(Duration::from_millis(10), |alert| {
    ///     eprintln!("Slow subscriber {:?} took {}ms",
    ///         alert.subscriber_id,
    ///         alert.duration_ns / 1_000_000);
    /// });
    /// ```
    #[cfg(feature = "metrics")]
    pub fn on_slow_subscriber<F>(&self, threshold: Duration, callback: F)
    where
        F: Fn(SlowSubscriberAlert) + Send + Sync + 'static,
    {
        self.inner
            .slow_subscriber_threshold_ns
            .store(Arc::new(Some(threshold.as_nanos() as u64)));
        self.inner
            .slow_subscriber_callback
            .store(Arc::new(Some(Arc::new(callback))));
    }

    /// Lock this mutable cell, converting it to an immutable cell.
    /// The underlying data is shared; only the type changes.
    pub fn lock(self) -> Cell<T, CellImmutable> {
        Cell {
            inner: self.inner,
            _marker: PhantomData,
        }
    }

    pub fn with_name(self, name: impl Into<Arc<str>>) -> Self {
        let name = name.into();
        *self.inner.name.lock().expect("cell name poisoned") = Some(name.clone());
        #[cfg(feature = "trace")]
        crate::tracing::update_name(self.inner.id, name.to_string());
        self
    }

    /// Check if the cell appears backed up based on last notify time.
    ///
    /// Returns true if metrics are enabled and the last notify took longer
    /// than 1ms (the default threshold). Use `is_backed_up_threshold()` for
    /// a custom threshold.
    ///
    /// Returns false if metrics are not enabled.
    #[cfg(feature = "metrics")]
    pub fn is_backed_up(&self) -> bool {
        self.is_backed_up_threshold(std::time::Duration::from_millis(1))
    }

    /// Check if the cell is backed up with a custom threshold.
    ///
    /// Returns true if metrics are enabled and the last notify duration
    /// exceeded the given threshold.
    #[cfg(feature = "metrics")]
    pub fn is_backed_up_threshold(&self, threshold: std::time::Duration) -> bool {
        self.inner
            .metrics
            .as_ref()
            .map(|m| m.last_notify_time_ns() > threshold.as_nanos() as u64)
            .unwrap_or(false)
    }

    /// Try to set a value, rejecting if the cell appears backed up.
    ///
    /// Uses the default 1ms threshold. Returns `Err(value)` if the cell
    /// is backed up (last notify took > 1ms), allowing the caller to
    /// handle backpressure.
    #[cfg(feature = "metrics")]
    pub fn try_set(&self, value: T) -> Result<(), T> {
        if self.is_backed_up() {
            Err(value)
        } else {
            self.set(value);
            Ok(())
        }
    }

    /// Try to set a value with a custom backpressure threshold.
    ///
    /// Returns `Err(value)` if the last notify duration exceeded the threshold.
    #[cfg(feature = "metrics")]
    pub fn try_set_threshold(&self, value: T, threshold: std::time::Duration) -> Result<(), T> {
        if self.is_backed_up_threshold(threshold) {
            Err(value)
        } else {
            self.set(value);
            Ok(())
        }
    }
}

impl<T, M> Clone for Cell<T, M> {
    fn clone(&self) -> Self {
        Cell {
            inner: Arc::clone(&self.inner),
            _marker: PhantomData,
        }
    }
}

impl<T, M> Cell<T, M> {
    /// Create a weak reference to this cell.
    /// The weak reference doesn't prevent the cell from being dropped.
    pub fn downgrade(&self) -> WeakCell<T, M> {
        WeakCell {
            inner: Arc::downgrade(&self.inner),
            _marker: PhantomData,
        }
    }

    /// Get metrics if enabled for this cell.
    ///
    /// Returns `None` if the cell was created without metrics.
    /// Use `Cell::with_metrics()` to create a cell with metrics enabled.
    #[cfg(feature = "metrics")]
    pub fn metrics(&self) -> Option<&CellMetrics> {
        self.inner.metrics.as_ref().map(|m| m.as_ref())
    }

    /// Take ownership of a subscription guard, dropping it when this cell is dropped.
    pub fn own(&self, guard: SubscriptionGuard) {
        #[cfg(feature = "inspector")]
        crate::registry::registry().mark_owned(guard.source().id(), self.inner.id);
        self.inner.owned.insert(Uuid::new_v4(), guard);
        // An added dependency edge invalidates cached scheduler heights.
        #[cfg(feature = "scheduler")]
        crate::scheduler::bump_topology_epoch();
        #[cfg(feature = "trace")]
        crate::tracing::update_owned_count(self.inner.id, self.inner.owned.len());
    }

    /// Take ownership of a subscription guard with a stable key.
    ///
    /// If a guard with the same key already exists, it is replaced (and dropped).
    /// This is used by `switch_map` to ensure the old inner subscription is cleaned up
    /// when switching to a new inner cell.
    pub fn own_keyed(&self, key: Uuid, guard: SubscriptionGuard) {
        #[cfg(feature = "inspector")]
        {
            // Unmark old owned cell if being replaced
            if let Some((_, old_guard)) = self.inner.owned.remove(&key) {
                crate::registry::registry().unmark_owned(old_guard.source().id());
            }
            crate::registry::registry().mark_owned(guard.source().id(), self.inner.id);
        }
        self.inner.owned.insert(key, guard);
        // switch_map rewiring changes edges (and heights); invalidate the cache.
        #[cfg(feature = "scheduler")]
        crate::scheduler::bump_topology_epoch();
        #[cfg(feature = "trace")]
        crate::tracing::update_owned_count(self.inner.id, self.inner.owned.len());
    }
}

// ============================================================================
// DepNode implementation for Cell - enables type-erased dependency traversal
// ============================================================================

#[cfg(feature = "scheduler")]
impl<T, M> Cell<T, M> {
    /// Opt this cell out of the scheduler's last-write-wins coalescing.
    ///
    /// Under [`batch`](crate::batch), a coalescing cell keeps only its final
    /// value per tick — correct for behavior operators (map/filter/join/
    /// switch_map), but it silently drops intermediates for event operators
    /// (scan/pairwise/merge/buffer/zip) and hand-rolled stateful maps, whose
    /// result depends on seeing every emission. Marking such a cell
    /// `no_coalesce` makes the scheduler enqueue each of its notifies as a
    /// distinct height-ordered op — every intermediate preserved, still drained
    /// in height order (so it reads settled inputs; deferral is glitch-free, only
    /// the last-write-wins *drop* is unsafe for these).
    ///
    /// For an event-semantic *subgraph*, prefer
    /// [`scheduler::no_coalesce`](crate::scheduler::no_coalesce), which stamps
    /// every cell born inside it — including the sources upstream of the
    /// operator, where coalescing would otherwise starve it before its inputs
    /// ever reach it. This builder is the single-cell escape hatch for sites
    /// where wrapping construction is awkward.
    pub fn no_coalesce(self) -> Self {
        self.inner.no_coalesce.store(true, Ordering::Relaxed);
        self
    }
}

impl<T: Send + Sync, M: Send + Sync> DepNode for Cell<T, M> {
    fn id(&self) -> Uuid {
        self.inner.id
    }

    fn name(&self) -> Option<String> {
        self.inner
            .name
            .lock()
            .expect("cell name poisoned")
            .as_ref()
            .map(|s| s.to_string())
    }

    fn deps(&self) -> Vec<Arc<dyn DepNode>> {
        // Collect unique dependencies from owned subscription guards
        let mut seen = std::collections::HashSet::new();
        self.inner
            .owned
            .iter()
            .filter_map(|entry| {
                let source = entry.value().source();
                let id = source.id();
                if seen.insert(id) {
                    Some(Arc::clone(source))
                } else {
                    None
                }
            })
            .collect()
    }

    #[cfg(feature = "scheduler")]
    fn height_cache(&self) -> Option<&std::sync::atomic::AtomicU64> {
        Some(&self.inner.height_cache)
    }

    #[cfg(feature = "scheduler")]
    fn no_coalesce(&self) -> bool {
        self.inner.no_coalesce.load(Ordering::Relaxed)
    }

    fn subscriber_count(&self) -> usize {
        self.inner.subscribers.lock().len() + self.inner.result_subscribers.lock().len()
    }

    fn owned_count(&self) -> usize {
        self.inner.owned.len()
    }
}

impl<T: CellValue> Cell<T, CellImmutable> {
    pub fn with_name(self, name: impl Into<Arc<str>>) -> Self {
        let name = name.into();
        *self.inner.name.lock().expect("cell name poisoned") = Some(name.clone());
        #[cfg(feature = "trace")]
        crate::tracing::update_name(self.inner.id, name.to_string());
        self
    }
}

impl<T: CellValue, M: Send + Sync + 'static> Cell<T, M> {
    /// Emit a signal to all subscribers.
    ///
    /// This is the unified notification mechanism for values, completion, and errors.
    ///
    /// Under the `profiling` feature the propagation boundaries
    /// ([`notify`](Self::notify)/[`write_value`](Self::write_value)/[`fanout`](Self::fanout))
    /// are `#[inline(never)]` so sampling profilers resolve them as distinct
    /// frames instead of folding the whole cascade into one `eq`/`notify`
    /// symbol. This costs a call on the hot path, so it is opt-in.
    #[doc(hidden)]
    #[cfg_attr(feature = "profiling", inline(never))]
    pub fn notify(&self, signal: Signal<T>) {
        // Don't emit anything after completion or error
        if self.inner.completed.load(Ordering::SeqCst) || self.inner.errored.load(Ordering::SeqCst)
        {
            return;
        }

        // Opt-in scheduler interception. Inside a `batch` (never in the
        // default build, never on the synchronous path) this defers the
        // value-settle + fanout into the height-ordered tick queue and returns;
        // the drain runs them in order at the batch boundary. One thread-local
        // bool load when the feature is on but no batch is open.
        #[cfg(feature = "scheduler")]
        if crate::scheduler::tick_active() {
            let cell = self.clone();
            let signal = signal.clone();
            crate::scheduler::enqueue(
                self.inner.id,
                self as &dyn crate::traits::DepNode,
                move || {
                    cell.write_value(&signal);
                    cell.fanout(&signal);
                },
            );
            return;
        }

        // Two phases, split so the (opt-in) scheduler can settle a cell's value
        // in height order *before* running its fanout — glitch-free coalescing —
        // and so sampling profilers resolve the value-write and the fanout as
        // distinct symbols instead of one folded `notify`. Outside a scheduler
        // batch (the default, and always on wasm) they run back-to-back: the
        // exact synchronous eager-push path, with no behavioral change.
        self.write_value(&signal);
        self.fanout(&signal);
    }

    /// Settle this cell's current value — or its terminal completed/errored
    /// state — from `signal`. Brief mutex work only; runs no subscriber fanout.
    #[cfg_attr(feature = "profiling", inline(never))]
    fn write_value(&self, signal: &Signal<T>) {
        match signal {
            Signal::Value(arc_value) => {
                // `Mutex<Arc<T>>` write: brief lock, swap the Arc, drop lock.
                // The previous Arc drops inline at the end of this scope —
                // `Arc::drop` is just a refcount decrement (and dealloc when
                // it hits zero), no `arc_swap::Debt::pay_all` reader-slot
                // scan. Readers that grabbed an earlier Arc keep it alive
                // via their own clone until they're done.
                *self.inner.value.lock().expect("cell value poisoned") = arc_value.clone();
            }
            Signal::Complete => {
                self.inner.completed.store(true, Ordering::SeqCst);
            }
            Signal::Error(err) => {
                self.inner.errored.store(true, Ordering::SeqCst);
                *self.inner.error.lock().expect("cell error poisoned") = Some(err.clone());
            }
        }
    }

    /// Fan `signal` out to this cell's subscribers. The value is assumed already
    /// settled by [`write_value`]; callbacks run with no internal lock held.
    #[cfg_attr(feature = "profiling", inline(never))]
    fn fanout(&self, signal: &Signal<T>) {
        // Tally this emit against the active measurement pass (if any). One per
        // fanout: synchronously this counts every re-fire; under `batch` the
        // coalesced cell fanouts once, so the same counter shows the collapse.
        // Pure measurement — compiles to nothing without `profiling`.
        #[cfg(feature = "profiling")]
        crate::profiling::record_fire(self.inner.id);

        // A `tracing` span per fanout so span-based profilers (`tracing-flame`,
        // `tracing-tracy`) get one entry per cell emit, tagged with the cell's
        // id and (if set) its name. The consumer attaches the subscriber; when
        // `profiling` is off this compiles to nothing. Later phases nest this
        // under a per-frame span.
        #[cfg(feature = "profiling")]
        let _fanout_span = {
            let name = self.inner.name.lock().expect("cell name poisoned").clone();
            ::tracing::trace_span!(
                "hyphae.fanout",
                cell.id = %self.inner.id,
                cell.name = name.as_deref().unwrap_or(""),
            )
            .entered()
        };

        // Start timing if metrics enabled
        #[cfg(feature = "metrics")]
        let notify_start = self
            .inner
            .metrics
            .as_ref()
            .map(|_| crate::platform::Instant::now());

        // Hot path: take the subscribers mutex briefly to grab the notify
        // snapshot (rebuilt from the id-index only if it changed since the last
        // notify), drop the lock, then iterate with no internal lock held.
        // Subscriber callbacks run lock-free; subscribers added during this
        // iteration land in the next notify's snapshot (they're inserted into
        // the index and mark it dirty; this in-flight notify iterates its
        // already-cloned snapshot). The displaced old snapshot drops *outside*
        // the lock — it may hold the last ref to an unsubscribed subscriber
        // whose drop cascades into upstream cell drops.
        let subs = {
            let (subs, old_snapshot) = self.inner.subscribers.lock().snapshot();
            drop(old_snapshot);
            subs
        };

        // Slow-subscriber config is only consulted when metrics are enabled
        // and configured. Defer the ArcSwap loads until then so the steady
        // state pays nothing.
        #[cfg(feature = "metrics")]
        let metrics = &self.inner.metrics;
        #[cfg(feature = "metrics")]
        let (slow_threshold, slow_callback) = if metrics.is_some() {
            (
                **self.inner.slow_subscriber_threshold_ns.load(),
                (**self.inner.slow_subscriber_callback.load()).clone(),
            )
        } else {
            (None, None)
        };

        // Subscriber callbacks must not panic — see `Watchable::subscribe` docs.
        // A panic here propagates out of the caller's `set`/`send` and halts the
        // rest of this fanout, which is a bug in the subscriber that should surface
        // loudly rather than be silently swallowed.
        for (_subscriber_id, sub) in subs.as_slice() {
            #[cfg(feature = "metrics")]
            let sub_start = metrics.as_ref().map(|_| crate::platform::Instant::now());

            (sub.callback)(signal);

            #[cfg(feature = "metrics")]
            if let (Some(m), Some(start)) = (metrics, sub_start) {
                let elapsed = start.elapsed().as_nanos() as u64;
                m.update_slowest_subscriber(elapsed);

                if let (Some(threshold), Some(cb)) = (&slow_threshold, &slow_callback)
                    && elapsed > *threshold
                {
                    let alert = SlowSubscriberAlert {
                        subscriber_id: *_subscriber_id,
                        duration_ns: elapsed,
                        threshold_ns: *threshold,
                    };
                    cb(alert);
                }
            }
        }

        // Fallible subscribers run after the infallible chain. Errors are logged
        // and dropped — they do not interrupt the fanout, and the panic contract
        // above still applies (a panic in a result-subscriber halts the rest of
        // this loop). Use `subscribe_result` when you want a structured error
        // channel instead of `panic!`.
        // Same snapshot pattern as `subscribers` above.
        let result_subs = {
            let (result_subs, old_snapshot) = self.inner.result_subscribers.lock().snapshot();
            drop(old_snapshot);
            result_subs
        };

        for (subscriber_id, sub) in result_subs.as_slice() {
            #[cfg(feature = "metrics")]
            let sub_start = metrics.as_ref().map(|_| crate::platform::Instant::now());

            if let Err(err) = (sub.callback)(signal) {
                log::error!(
                    "hyphae: fallible subscriber {} on cell {} returned error: {}",
                    subscriber_id,
                    self.inner.id,
                    err
                );
            }

            #[cfg(feature = "metrics")]
            if let (Some(m), Some(start)) = (metrics, sub_start) {
                let elapsed = start.elapsed().as_nanos() as u64;
                m.update_slowest_subscriber(elapsed);

                if let (Some(threshold), Some(cb)) = (&slow_threshold, &slow_callback)
                    && elapsed > *threshold
                {
                    let alert = SlowSubscriberAlert {
                        subscriber_id: *subscriber_id,
                        duration_ns: elapsed,
                        threshold_ns: *threshold,
                    };
                    cb(alert);
                }
            }
        }

        // Record overall notify timing
        #[cfg(feature = "metrics")]
        if let (Some(metrics), Some(start)) = (&self.inner.metrics, notify_start) {
            let duration_ns = start.elapsed().as_nanos() as u64;
            metrics.record_notify(duration_ns);
            #[cfg(feature = "trace")]
            crate::tracing::record_notify(
                self.inner.id,
                duration_ns,
                subs.as_slice().len() + result_subs.as_slice().len(),
                self.inner.owned.len(),
                metrics.slowest_subscriber_ns(),
            );
        }
    }
}

impl<T: CellValue, U: Send + Sync + 'static> Gettable<T> for Cell<T, U> {
    fn get(&self) -> T {
        // Brief lock to clone the Arc (refcount bump), release, then deref
        // and clone T outside the lock. Keeps the critical section small.
        let arc = self
            .inner
            .value
            .lock()
            .expect("cell value poisoned")
            .clone();
        (*arc).clone()
    }
}

impl<T: CellValue, U: Send + Sync + 'static> Watchable<T> for Cell<T, U> {
    fn subscribe(
        &self,
        callback: impl Fn(&Signal<T>) + Send + Sync + 'static,
    ) -> SubscriptionGuard {
        let id = Uuid::new_v4();
        let sub = Arc::new(Subscriber::new(callback));

        // Insert BEFORE seeding. The prior order (fire the seed with the current
        // value, THEN insert) left a window in which a concurrent `notify` on
        // another thread could take its subscriber snapshot between the seed and
        // the insert: that notify iterated a snapshot WITHOUT this subscriber,
        // so the new subscriber missed the emit and latched the stale seed value
        // with no way to recover until some later emit reached it. Against a
        // source that had already moved on, the subscription stranded
        // permanently — correct on a fresh `get()` (reads live value) but stuck
        // on the live subscription. That is the root of the intermittent
        // "value stuck, UI/fresh-read correct, clears on restart" class.
        //
        // Inserting first guarantees this subscriber is in the index for every
        // subsequent notify, so it can never miss the source moving on. The seed
        // below then only needs to backfill the current value.
        //
        // Any displaced Arc (none, for a fresh id) drops *outside* the lock: a
        // subscriber's drop can cascade into upstream cell drops that touch
        // other cell mutexes, and running that under this lock allowed two
        // concurrently-dropping cells to deadlock.
        let displaced = self.inner.subscribers.lock().insert(id, sub.clone());
        drop(displaced);

        // Seed the current value AFTER the insert (backfilling the freshest
        // stored value) and fire OUTSIDE the subscribers lock — subscriber
        // callbacks must never run with an internal cell mutex held, since they
        // can cascade into other cells' locks/drops and deadlock. A notify that
        // raced the insert above already delivers to this now-indexed
        // subscriber; a duplicate value delivery is benign, and any one-emit
        // ordering skew self-heals on the next notify.
        let current = self
            .inner
            .value
            .lock()
            .expect("cell value poisoned")
            .clone();
        (sub.callback)(&Signal::Value(current));

        // If already complete or errored, send that signal too
        if self.is_complete() {
            (sub.callback)(&Signal::Complete);
        } else if self.is_error()
            && let Some(err) = self.error()
        {
            (sub.callback)(&Signal::Error(err));
        }

        // Record subscriber added if metrics enabled
        #[cfg(feature = "metrics")]
        if let Some(metrics) = &self.inner.metrics {
            metrics.record_subscriber_added();
        }
        #[cfg(feature = "trace")]
        {
            let subs_len = self.inner.subscribers.lock().len();
            let result_len = self.inner.result_subscribers.lock().len();
            crate::tracing::update_subscriber_count(self.inner.id, subs_len + result_len);
        }

        let source: Arc<dyn DepNode> = Arc::new(self.clone());
        let cell = self.clone();
        #[cfg(feature = "metrics")]
        let metrics = self.inner.metrics.clone();
        SubscriptionGuard::new(id, source, move || {
            // O(1) indexed remove; the removed subscriber drops outside the lock
            // (see insert above).
            let removed_sub = cell.inner.subscribers.lock().remove(&id);
            let removed = removed_sub.is_some();
            drop(removed_sub);
            #[cfg(feature = "metrics")]
            if removed && let Some(m) = &metrics {
                m.record_subscriber_removed();
            }
            #[cfg(not(feature = "metrics"))]
            let _ = removed;
            #[cfg(feature = "trace")]
            {
                let subs_len = cell.inner.subscribers.lock().len();
                let result_len = cell.inner.result_subscribers.lock().len();
                crate::tracing::update_subscriber_count(cell.inner.id, subs_len + result_len);
            }
        })
    }

    fn unsubscribe(&self, id: Uuid) {
        // O(1) indexed removes. The removed `Arc`s drop AFTER each lock guard is
        // released so cascading Subscriber/Cell Drops never run with an internal
        // cell mutex held (two concurrently-dropping cells could otherwise
        // acquire each other's mutex and deadlock).
        let removed_sub = self.inner.subscribers.lock().remove(&id);
        let removed_from_subs = removed_sub.is_some();
        drop(removed_sub);
        let removed_from_result = if removed_from_subs {
            false
        } else {
            let removed = self.inner.result_subscribers.lock().remove(&id);
            let did = removed.is_some();
            drop(removed);
            did
        };
        if removed_from_subs || removed_from_result {
            // Record subscriber removed if metrics enabled
            #[cfg(feature = "metrics")]
            if let Some(metrics) = &self.inner.metrics {
                metrics.record_subscriber_removed();
            }
            #[cfg(feature = "trace")]
            {
                let subs_len = self.inner.subscribers.lock().len();
                let result_len = self.inner.result_subscribers.lock().len();
                crate::tracing::update_subscriber_count(self.inner.id, subs_len + result_len);
            }
        }
    }

    fn is_complete(&self) -> bool {
        self.inner.completed.load(Ordering::SeqCst)
    }

    fn is_error(&self) -> bool {
        self.inner.errored.load(Ordering::SeqCst)
    }

    fn error(&self) -> Option<Arc<anyhow::Error>> {
        self.inner
            .error
            .lock()
            .expect("cell error poisoned")
            .clone()
    }
}

impl<T: CellValue, U: Send + Sync + 'static> WatchableResult<T> for Cell<T, U> {
    fn subscribe_result(
        &self,
        callback: impl Fn(&Signal<T>) -> Result<(), String> + Send + Sync + 'static,
    ) -> SubscriptionGuard {
        let cell_id = self.inner.id;
        let log_err = |id: &Uuid, err: &str| {
            log::error!(
                "hyphae: fallible subscriber {} on cell {} returned error: {}",
                id,
                cell_id,
                err
            );
        };

        let id = Uuid::new_v4();

        // Send current value immediately (Arc clone, no deep copy).
        let current = self
            .inner
            .value
            .lock()
            .expect("cell value poisoned")
            .clone();
        if let Err(err) = callback(&Signal::Value(current)) {
            log_err(&id, &err);
        }

        // Replay any prior terminal signal.
        if self.inner.completed.load(Ordering::SeqCst) {
            if let Err(err) = callback(&Signal::Complete) {
                log_err(&id, &err);
            }
        } else if self.inner.errored.load(Ordering::SeqCst)
            && let Some(e) = self
                .inner
                .error
                .lock()
                .expect("cell error poisoned")
                .clone()
            && let Err(err) = callback(&Signal::Error(e))
        {
            log_err(&id, &err);
        }

        let sub = Arc::new(ResultSubscriber::new(callback));
        // O(1) indexed insert; displaced Arc drops outside the lock. See
        // Watchable::subscribe above.
        let displaced = self.inner.result_subscribers.lock().insert(id, sub);
        drop(displaced);

        #[cfg(feature = "metrics")]
        if let Some(metrics) = &self.inner.metrics {
            metrics.record_subscriber_added();
        }
        #[cfg(feature = "trace")]
        {
            let subs_len = self.inner.subscribers.lock().len();
            let result_len = self.inner.result_subscribers.lock().len();
            crate::tracing::update_subscriber_count(self.inner.id, subs_len + result_len);
        }

        let source: Arc<dyn DepNode> = Arc::new(self.clone());
        let cell = self.clone();
        #[cfg(feature = "metrics")]
        let metrics = self.inner.metrics.clone();
        SubscriptionGuard::new(id, source, move || {
            // O(1) indexed remove; removed subscriber drops outside the lock.
            // See Watchable::subscribe above.
            let removed_sub = cell.inner.result_subscribers.lock().remove(&id);
            let removed = removed_sub.is_some();
            drop(removed_sub);
            #[cfg(feature = "metrics")]
            if removed && let Some(m) = &metrics {
                m.record_subscriber_removed();
            }
            #[cfg(not(feature = "metrics"))]
            let _ = removed;
            #[cfg(feature = "trace")]
            {
                let subs_len = cell.inner.subscribers.lock().len();
                let result_len = cell.inner.result_subscribers.lock().len();
                crate::tracing::update_subscriber_count(cell.inner.id, subs_len + result_len);
            }
        })
    }
}

impl<T: CellValue> Mutable<T> for Cell<T, CellMutable> {
    fn set(&self, value: T) {
        self.notify(Signal::value(value)); // Wraps in Arc
    }

    fn complete(&self) {
        self.notify(Signal::Complete);
    }

    fn fail(&self, error: impl Into<anyhow::Error>) {
        self.notify(Signal::error(error));
    }
}

// ============================================================================
// Inspector feature: DepNode for CellInner + Drop to deregister
// ============================================================================

#[cfg(feature = "inspector")]
impl<T: CellValue> DepNode for CellInner<T> {
    fn id(&self) -> Uuid {
        self.id
    }

    fn name(&self) -> Option<String> {
        self.name
            .lock()
            .expect("cell name poisoned")
            .as_ref()
            .map(|s| s.to_string())
    }

    fn deps(&self) -> Vec<Arc<dyn DepNode>> {
        let mut seen = std::collections::HashSet::new();
        self.owned
            .iter()
            .filter_map(|entry| {
                let source = entry.value().source();
                let id = source.id();
                if seen.insert(id) {
                    Some(Arc::clone(source))
                } else {
                    None
                }
            })
            .collect()
    }

    fn subscriber_count(&self) -> usize {
        self.subscribers.lock().len() + self.result_subscribers.lock().len()
    }

    fn owned_count(&self) -> usize {
        self.owned.len()
    }

    fn value_debug(&self) -> Option<String> {
        let arc = self.value.lock().expect("cell value poisoned").clone();
        Some(format!("{:?}", &*arc))
    }

    fn caller(&self) -> Option<&'static Location<'static>> {
        Some(self.caller)
    }
}

impl<T> Drop for CellInner<T> {
    fn drop(&mut self) {
        #[cfg(feature = "trace")]
        crate::tracing::deregister_cell(&self.id);
        #[cfg(feature = "inspector")]
        crate::registry::registry().deregister(&self.id);
    }
}

#[cfg(all(feature = "metrics", feature = "trace"))]
fn default_metrics() -> Option<Arc<CellMetrics>> {
    Some(Arc::new(CellMetrics::new()))
}

#[cfg(all(feature = "metrics", not(feature = "trace")))]
fn default_metrics() -> Option<Arc<CellMetrics>> {
    None
}

#[cfg(test)]
mod sub_index_tests {
    use super::{SubIndex, SubSnapshot};
    use std::sync::Arc;
    use uuid::Uuid;

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
        let displaced = idx.insert(id, sub(2)).expect("old sub returned");
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

        let removed = idx.remove(&id).expect("present");
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
