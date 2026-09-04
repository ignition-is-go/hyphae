use std::{
    fmt::Debug,
    marker::PhantomData,
    panic::Location,
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, Ordering},
    },
};

use dashmap::DashMap;
use parking_lot::Mutex;
use uuid::Uuid;

use crate::{
    signal::Signal,
    subscription::SubscriptionGuard,
    traits::{CellValue, DepNode, Gettable, Mutable, Watchable, WatchableResult},
};

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
    /// `ArcSwap<T>` so writes don't pay `arc_swap`'s reader-debt-slot scan.
    /// Reads `lock + clone (Arc bump) + unlock`. Writes
    /// `lock + assign (drops old Arc inline) + unlock`. Old values reclaim
    /// via `Arc` refcounting — readers holding clones keep the value alive
    /// until they drop.
    pub(crate) value: Mutex<Arc<T>>,
    /// Optional human-readable name for tracing/debugging. Cold path — set
    /// rarely via `with_name`, read from `DepNode::name`. Mutex avoids the
    /// per-cell `ArcSwap` drop cost paid on every cell teardown.
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
    /// Per-node height epoch for localized cache invalidation. Bumped whenever
    /// an edge change in this cell's transitive-dependency cone could change its
    /// height; `height_cache` is tagged with the epoch it was computed under and
    /// recomputed only when they differ. Starts at 1 so a zero-initialized
    /// `height_cache` reads as stale. Replaces the old process-global topology
    /// epoch (which flushed *every* cached height on any edge change — pathologically
    /// costly during a topology-churning knit).
    #[cfg(feature = "scheduler")]
    pub(crate) height_epoch: std::sync::atomic::AtomicU64,
    /// Weak back-edges to the cells whose height depends (transitively) on this
    /// one — the invalidation cone walked when this cell's deps change (see
    /// [`invalidate_height_cone`]). Weak so a dependent's death doesn't pin it
    /// here; the walk prunes dead entries. May over-approximate (a stale entry
    /// only causes a harmless extra recompute) but must never miss a live
    /// dependent, or that dependent would keep a stale height (a glitch).
    #[cfg(feature = "scheduler")]
    pub(crate) height_dependents: Mutex<Vec<std::sync::Weak<dyn HeightInvalidate>>>,
    /// Source location where this cell was created (via #[`track_caller`]).
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
    #[must_use]
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
    #[must_use]
    pub fn is_alive(&self) -> bool {
        self.inner.strong_count() > 0
    }
}

impl<T, M> Clone for WeakCell<T, M> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            _marker: PhantomData,
        }
    }
}

mod subscriber;

pub(crate) use subscriber::{ResultSubscriber, Subscriber, SubscriberCallback, SubscriberRegistry};

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
            #[cfg(feature = "scheduler")]
            height_epoch: std::sync::atomic::AtomicU64::new(1),
            #[cfg(feature = "scheduler")]
            height_dependents: Mutex::new(Vec::new()),
            caller: Location::caller(),
        });
        Self {
            inner,
            _marker: PhantomData,
        }
    }

    /// Lock this mutable cell, converting it to an immutable cell.
    /// The underlying data is shared; only the type changes.
    #[must_use]
    pub fn lock(self) -> Cell<T, CellImmutable> {
        Cell {
            inner: self.inner,
            _marker: PhantomData,
        }
    }

    #[must_use]
    pub fn with_name(self, name: impl Into<Arc<str>>) -> Self {
        let name = name.into();
        *self.inner.name.lock() = Some(name);
        self
    }
}

impl<T, M> Clone for Cell<T, M> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
            _marker: PhantomData,
        }
    }
}

impl<T, M> Cell<T, M> {
    /// Create a weak reference to this cell.
    /// The weak reference doesn't prevent the cell from being dropped.
    #[must_use]
    pub fn downgrade(&self) -> WeakCell<T, M> {
        WeakCell {
            inner: Arc::downgrade(&self.inner),
            _marker: PhantomData,
        }
    }

    /// Take ownership of a subscription guard, dropping it when this cell is dropped.
    ///
    /// # Do not capture this cell strongly in the subscription's closure
    ///
    /// A [`SubscriptionGuard`] holds a **strong** `Arc` to the source it
    /// subscribed to. So if the closure you subscribed with also holds a strong
    /// clone of the cell you then `own` the guard on, you have built a cycle:
    ///
    /// ```text
    /// CellInner -> owned guard -> Arc<source> -> subscribers -> closure -> CellInner
    /// ```
    ///
    /// Nothing breaks that cycle. The cell never drops, so it never
    /// unsubscribes; the closure keeps firing forever, recomputing a value no
    /// one can observe. It is a CPU leak as well as a memory one, and it is
    /// invisible in tests that only assert on values.
    ///
    /// Measured: ~17 KB retained per created-and-dropped cell, versus ~0 with
    /// the weak form below.
    ///
    /// Capture a [`WeakCell`] and upgrade inside the closure instead:
    ///
    /// ```rust
    /// # use hyphae::{Cell, Gettable, Mutable, Watchable, Signal};
    /// let source = Cell::new(1i32);
    /// let out = Cell::new(0i32);
    ///
    /// let weak = out.downgrade();            // <- weak, not `out.clone()`
    /// let guard = source.subscribe(move |signal| {
    ///     if let Signal::Value(v) = signal
    ///         && let Some(out) = weak.upgrade()
    ///     {
    ///         out.set(**v * 2);
    ///     }
    /// });
    /// out.own(guard);
    /// # source.set(21);
    /// # assert_eq!(out.get(), 42);
    /// ```
    ///
    /// If the cell genuinely must outlive its own scope, keep it alive by
    /// holding it somewhere real — not by making its own subscription pin it.
    /// A self-pinning cell is indistinguishable from a leak, because that is
    /// what it is.
    pub fn own(&self, guard: SubscriptionGuard)
    where
        T: Send + Sync + 'static,
    {
        // Register this cell as a height-dependent of the guard's source (so a
        // later edge change there invalidates our cached height), then invalidate
        // our own height cone — our dependency set just changed. Localized: only
        // this cell and its transitive dependents recompute, not the whole graph.
        #[cfg(feature = "scheduler")]
        {
            let erased: Arc<dyn HeightInvalidate> = self.inner.clone();
            let dep = Arc::downgrade(&erased);
            guard.source().add_height_dependent(dep);
        }
        self.inner.owned.insert(Uuid::new_v4(), guard);
        #[cfg(feature = "scheduler")]
        invalidate_height_cone(self.inner.as_ref());
    }

    /// Take ownership of a subscription guard with a stable key.
    ///
    /// If a guard with the same key already exists, it is replaced (and dropped).
    /// This is used by `switch_map` to ensure the old inner subscription is cleaned up
    /// when switching to a new inner cell.
    pub fn own_keyed(&self, key: Uuid, guard: SubscriptionGuard)
    where
        T: Send + Sync + 'static,
    {
        // Register as a height-dependent of the new source before it moves into
        // `owned`. The replaced guard's stale back-edge on the *old* source is
        // left to be pruned lazily — a stale dependent only over-invalidates (a
        // harmless extra recompute), never under-invalidates.
        #[cfg(feature = "scheduler")]
        {
            let erased: Arc<dyn HeightInvalidate> = self.inner.clone();
            let dep = Arc::downgrade(&erased);
            guard.source().add_height_dependent(dep);
        }
        self.inner.owned.insert(key, guard);
        // switch_map rewiring changed our dep set: invalidate our height cone
        // (this cell + its transitive dependents), not the whole process.
        #[cfg(feature = "scheduler")]
        invalidate_height_cone(self.inner.as_ref());
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
    /// `switch_map`), but it silently drops intermediates for event operators
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
    #[must_use]
    pub fn no_coalesce(self) -> Self {
        self.inner.no_coalesce.store(true, Ordering::Relaxed);
        self
    }
}

/// Type-erased handle to a node participating in localized height-cache
/// invalidation. Implemented on the stable `Arc<CellInner<T>>` (not the ephemeral
/// `Cell` handle that `DepNode` rides), so a `Weak<dyn HeightInvalidate>` can name
/// a specific cell for its lifetime — the identity the dependency back-edges need
/// and `DepNode` can't provide.
///
/// `pub` only because it appears in the (public) `DepNode::add_height_dependent`
/// signature; it is an internal scheduler detail, not stable API.
#[doc(hidden)]
#[cfg(feature = "scheduler")]
pub trait HeightInvalidate: Send + Sync {
    fn hi_id(&self) -> Uuid;
    /// Advance this node's height epoch, marking its cached height stale.
    fn hi_bump_epoch(&self);
    /// Snapshot the live dependents (pruning dead weaks) for the cone walk.
    fn hi_dependents(&self) -> Vec<std::sync::Weak<dyn HeightInvalidate>>;
}

#[cfg(feature = "scheduler")]
impl<T: Send + Sync> HeightInvalidate for CellInner<T> {
    fn hi_id(&self) -> Uuid {
        self.id
    }
    fn hi_bump_epoch(&self) {
        self.height_epoch.fetch_add(1, Ordering::Relaxed);
    }
    fn hi_dependents(&self) -> Vec<std::sync::Weak<dyn HeightInvalidate>> {
        let mut g = self.height_dependents.lock();
        // Amortized prune: a dropped dependent leaves a dead weak; sweep them on
        // read. NOTE: this bounds the vec to the live dependent set only because
        // `add_height_dependent` also rejects duplicates — retain alone cannot
        // remove a repeat registration of a still-live dependent (rship lv-c065).
        g.retain(|w| w.strong_count() > 0);
        g.clone()
    }
}

/// Invalidate the height cache of `start` and every node whose height depends
/// (transitively) on it — the downstream cone. Called when `start`'s dependency
/// set changes (`own`/`own_keyed`), so exactly the heights that could have moved
/// are recomputed on their next read, leaving unrelated (and already-settled)
/// subgraphs cached.
///
/// Bounds/safety: the visited set breaks cycles and de-dups diamonds; dead weaks
/// (a dependent dropped concurrently — e.g. a `switch_map` inner being torn down
/// in the same wave that invalidates it) upgrade to `None` and are skipped. The
/// walk may over-invalidate (a stale weak still upgrades until pruned → one extra
/// recompute) but never under-invalidates a live dependent.
#[cfg(feature = "scheduler")]
pub(crate) fn invalidate_height_cone(start: &dyn HeightInvalidate) {
    start.hi_bump_epoch();
    let mut visited = std::collections::HashSet::new();
    visited.insert(start.hi_id());
    let mut stack = start.hi_dependents();
    while let Some(weak) = stack.pop() {
        let Some(node) = weak.upgrade() else { continue };
        if visited.insert(node.hi_id()) {
            node.hi_bump_epoch();
            stack.extend(node.hi_dependents());
        }
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
            .as_ref()
            .map(std::string::ToString::to_string)
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
    fn height_epoch(&self) -> Option<&std::sync::atomic::AtomicU64> {
        Some(&self.inner.height_epoch)
    }

    #[cfg(feature = "scheduler")]
    fn add_height_dependent(&self, dep: std::sync::Weak<dyn HeightInvalidate>) {
        let mut deps = self.inner.height_dependents.lock();
        // Prune dead entries, then register only if this dependent isn't already
        // here. Both steps are load-bearing: `own`/`own_keyed` re-register the
        // SAME (source, dependent) pair on every `switch_map` re-knit whose
        // closure returns a CACHED cell (myko memoizes report/query cells, so
        // the "new" inner cell is usually the same live one). An unguarded push
        // therefore grew this Vec without bound at 16 bytes per re-knit —
        // measured at ~7.5k re-knits/s in the rship server, and quadratic in
        // allocator churn on top, because `hi_dependents` clones the whole Vec
        // on every invalidation. `retain(strong_count > 0)` alone did NOT bound
        // it: it only drops dependents that have DIED, never duplicates of a
        // live one. (rship lv-c065)
        deps.retain(|w| w.strong_count() > 0);
        // Compare data addresses, not `Weak::ptr_eq`: these are trait-object
        // weaks and vtable identity is not guaranteed across coercion sites.
        let new_addr = dep.as_ptr().cast::<()>();
        if deps.iter().any(|w| w.as_ptr().cast::<()>() == new_addr) {
            return;
        }
        deps.push(dep);
    }

    #[cfg(feature = "scheduler")]
    fn no_coalesce(&self) -> bool {
        self.inner.no_coalesce.load(Ordering::Relaxed)
    }

    fn subscriber_count(&self) -> usize {
        self.inner
            .subscribers
            .lock()
            .len()
            .saturating_add(self.inner.result_subscribers.lock().len())
    }

    fn owned_count(&self) -> usize {
        self.inner.owned.len()
    }
}

impl<T: CellValue> Cell<T, CellImmutable> {
    #[must_use]
    pub fn with_name(self, name: impl Into<Arc<str>>) -> Self {
        let name = name.into();
        *self.inner.name.lock() = Some(name);
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
            // A terminal signal must not coalesce over a value it follows (an
            // emit-then-complete operator would otherwise lose its final value
            // under `batch`) — see [`crate::scheduler::enqueue`].
            let terminal = !matches!(signal, Signal::Value(_));
            let signal = signal;
            let dependency: &dyn crate::traits::DepNode = self;
            crate::scheduler::enqueue(
                self.inner.id,
                dependency,
                terminal,
                Box::new(move || {
                    cell.write_value(&signal);
                    cell.fanout(&signal);
                }),
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
        // `notify` owns its signal because scheduler-enabled builds may move it
        // into the deferred tick closure. End the synchronous signal lifetime
        // explicitly as well so feature-minimal dependent builds preserve the
        // same ownership contract.
        drop(signal);
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
                *self.inner.value.lock() = arc_value.clone();
            }
            Signal::Complete => {
                self.inner.completed.store(true, Ordering::SeqCst);
            }
            Signal::Error(err) => {
                self.inner.errored.store(true, Ordering::SeqCst);
                *self.inner.error.lock() = Some(err.clone());
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
            let name = self.inner.name.lock().clone();
            ::tracing::trace_span!(
                "hyphae.fanout",
                cell.id = %self.inner.id,
                cell.name = name.as_deref().unwrap_or(""),
            )
            .entered()
        };

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

        // Subscriber callbacks must not panic — see `Watchable::subscribe` docs.
        // A panic here propagates out of the caller's `set`/`send` and halts the
        // rest of this fanout, which is a bug in the subscriber that should surface
        // loudly rather than be silently swallowed.
        for (_subscriber_id, sub) in subs.as_slice() {
            sub.deliver(signal);
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
            if let Err(err) = sub.deliver(signal) {
                log::error!(
                    "hyphae: fallible subscriber {} on cell {} returned error: {}",
                    subscriber_id,
                    self.inner.id,
                    err
                );
            }
        }
    }
}

impl<T: CellValue, U: Send + Sync + 'static> Gettable<T> for Cell<T, U> {
    fn get(&self) -> T {
        // Brief lock to clone the Arc (refcount bump), release, then deref
        // and clone T outside the lock. Keeps the critical section small.
        let arc = self.inner.value.lock().clone();
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
        let current = self.inner.value.lock().clone();
        sub.finish_initial(&Signal::Value(current));

        // If already complete or errored, send that signal too
        if self.is_complete() {
            sub.deliver(&Signal::Complete);
        } else if self.is_error()
            && let Some(err) = self.error()
        {
            sub.deliver(&Signal::Error(err));
        }

        let source: Arc<dyn DepNode> = Arc::new(self.clone());
        let cell = self.clone();
        SubscriptionGuard::new(id, source, move || {
            // O(1) indexed remove; the removed subscriber and displaced stale
            // snapshot both drop outside the lock (see insert above).
            let (removed_sub, stale_snap) = cell.inner.subscribers.lock().remove(&id);
            drop(removed_sub);
            drop(stale_snap);
        })
    }

    fn unsubscribe(&self, id: Uuid) {
        // O(1) indexed removes. The removed `Arc`s drop AFTER each lock guard is
        // released so cascading Subscriber/Cell Drops never run with an internal
        // cell mutex held (two concurrently-dropping cells could otherwise
        // acquire each other's mutex and deadlock).
        let (removed_sub, stale_snap) = self.inner.subscribers.lock().remove(&id);
        let removed_from_subs = removed_sub.is_some();
        drop(removed_sub);
        drop(stale_snap);
        if !removed_from_subs {
            let (removed, stale_result_snap) = self.inner.result_subscribers.lock().remove(&id);
            drop(removed);
            drop(stale_result_snap);
        }
    }

    fn is_complete(&self) -> bool {
        self.inner.completed.load(Ordering::SeqCst)
    }

    fn is_error(&self) -> bool {
        self.inner.errored.load(Ordering::SeqCst)
    }

    fn error(&self) -> Option<Arc<anyhow::Error>> {
        self.inner.error.lock().clone()
    }
}

impl<T: CellValue, U: Send + Sync + 'static> WatchableResult<T> for Cell<T, U> {
    fn subscribe_result(
        &self,
        callback: impl Fn(&Signal<T>) -> Result<(), String> + Send + Sync + 'static,
    ) -> SubscriptionGuard {
        let cell_id = self.inner.id;
        let log_err = |id: &Uuid, err: &str| {
            log::error!("hyphae: fallible subscriber {id} on cell {cell_id} returned error: {err}");
        };

        let id = Uuid::new_v4();
        let sub = Arc::new(ResultSubscriber::new(callback));

        // Match the infallible subscribe path: install before replay so an
        // update racing subscription cannot fall between the replay and the
        // registry insert. Result callbacks use the same initialization queue
        // to preserve replay-before-live ordering.
        let displaced = self.inner.result_subscribers.lock().insert(id, sub.clone());
        drop(displaced);

        // Send current value immediately (Arc clone, no deep copy).
        let current = self.inner.value.lock().clone();
        sub.finish_initial(&Signal::Value(current), |error| log_err(&id, error));

        // Replay any prior terminal signal.
        if self.inner.completed.load(Ordering::SeqCst) {
            if let Err(err) = sub.deliver(&Signal::Complete) {
                log_err(&id, &err);
            }
        } else if self.inner.errored.load(Ordering::SeqCst)
            && let Some(e) = self.inner.error.lock().clone()
            && let Err(err) = sub.deliver(&Signal::Error(e))
        {
            log_err(&id, &err);
        }

        let source: Arc<dyn DepNode> = Arc::new(self.clone());
        let cell = self.clone();
        SubscriptionGuard::new(id, source, move || {
            // O(1) indexed remove; removed subscriber and stale snapshot drop
            // outside the lock. See Watchable::subscribe above.
            let (removed_sub, stale_snap) = cell.inner.result_subscribers.lock().remove(&id);
            drop(removed_sub);
            drop(stale_snap);
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

impl<T> Drop for CellInner<T> {
    fn drop(&mut self) {}
}

#[cfg(all(test, feature = "scheduler"))]
mod height_dependents_tests {
    use crate::{Cell, Materialize, Mutable, SwitchMapExt, Watchable};

    /// Re-owning the SAME live source must not grow the source's
    /// height-dependent set. This is the `switch_map`-onto-a-cached-cell shape:
    /// the closure returns a memoized inner cell, so every re-knit calls
    /// `own_keyed` with an identical (source, dependent) pair. Before the dedup
    /// in `add_height_dependent`, each call pushed another 16-byte `Weak` that
    /// nothing could ever reclaim while both cells were alive — an unbounded
    /// leak proportional to re-knit rate (rship lv-c065).
    #[test]
    fn repeated_own_of_same_live_source_does_not_grow_dependents() {
        let source = Cell::new(0i32);
        let owner = Cell::new(0i32);
        let key = uuid::Uuid::new_v4();

        for _ in 0..1000 {
            let guard = source.subscribe(|_| {});
            owner.own_keyed(key, guard);
        }

        let len = source.inner.height_dependents.lock().len();
        assert_eq!(
            len, 1,
            "same (source, dependent) pair re-owned 1000x should register once, got {len}"
        );
    }

    /// Distinct dependents must still all be registered — the dedup must not
    /// under-approximate, or a dependent would keep a stale height (a glitch).
    #[test]
    fn distinct_dependents_are_all_registered() {
        let source = Cell::new(0i32);
        let owners: Vec<_> = (0..5).map(|_| Cell::new(0i32)).collect();
        for owner in &owners {
            owner.own(source.subscribe(|_| {}));
        }

        let len = source.inner.height_dependents.lock().len();
        assert_eq!(len, 5, "each distinct dependent must register exactly once");
    }

    /// A dead dependent's entry is still reclaimed (the pre-existing retain
    /// behavior must survive the dedup change).
    #[test]
    fn dead_dependents_are_pruned() {
        let source = Cell::new(0i32);
        {
            let owner = Cell::new(0i32);
            owner.own(source.subscribe(|_| {}));
        }
        // Force a read, which runs the amortized prune.
        let owner2 = Cell::new(0i32);
        owner2.own(source.subscribe(|_| {}));

        let len = source.inner.height_dependents.lock().len();
        assert_eq!(len, 1, "dropped dependent should not linger, got {len}");
    }

    /// End-to-end: a real `switch_map` re-knitting onto a shared long-lived
    /// inner cell (the myko report-cache shape) must not grow the inner cell's
    /// dependent set as the outer fires.
    #[test]
    fn switch_map_onto_shared_inner_does_not_grow() {
        let outer = Cell::new(0i32);
        let shared_inner = Cell::new(100i32);
        let inner_for_closure = shared_inner.clone();
        let switched = outer
            .clone()
            .switch_map(move |_| inner_for_closure.clone().lock())
            .materialize();
        let _guard = switched.subscribe(|_| {});

        for i in 1..500 {
            outer.set(i);
        }

        let len = shared_inner.inner.height_dependents.lock().len();
        assert!(
            len <= 2,
            "switch_map re-knit onto a cached inner cell grew dependents to {len}"
        );
    }
}
