use std::{
    fmt::Debug,
    marker::PhantomData,
    panic::Location,
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, Ordering},
    },
};

use parking_lot::Mutex;
use rustc_hash::FxHashMap;
use uuid::Uuid;

use crate::{
    settle::{Node, Stamp, stamp},
    signal::Signal,
    subscription::SubscriptionGuard,
    traits::{CellValue, DepNode, Gettable, Mutable, Watchable, WatchableResult},
};

#[derive(Debug, Clone)]
pub struct CellMutable;

#[derive(Debug, Clone)]
pub struct CellImmutable;

/// The inner data of a Cell, wrapped in Arc for shared ownership.
/// A single fanout longer than this is reported in debug builds.
#[cfg(debug_assertions)]
const SLOW_FANOUT: std::time::Duration = std::time::Duration::from_secs(1);

/// What a value does when its cell already holds a pending value in the
/// settle in progress.
pub(crate) enum Combine<T> {
    /// Both values are delivered, in order, even when both come from
    /// `Mutable::set`.
    Each,
    /// The new value replaces the pending one. The cell fans out once.
    Latest,
    /// The new value is folded into the pending one and the cell fans out
    /// once. The fold returns the new value when the two cannot be folded,
    /// and then both are delivered.
    Fold(fn(&mut Arc<T>, Arc<T>) -> Option<Arc<T>>),
}

/// Where a value that joins a settle goes in its cell's pending list.
#[derive(Clone, Copy)]
enum Order {
    /// Behind everything that arrived before it. For state: the writer
    /// changed its own state first, and the values must follow that order.
    Arrival,
    /// Where a one-thread settle would have delivered it. For events.
    Settle,
}

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
    pub(crate) owned: Mutex<FxHashMap<Uuid, SubscriptionGuard>>,
    /// Whether this cell has completed (no more values will be emitted).
    pub(crate) completed: AtomicBool,
    /// Whether this cell has errored.
    pub(crate) errored: AtomicBool,
    /// The error, if any. Cold path — only written when the cell errors,
    /// read by error/subscribe paths.
    pub(crate) error: Mutex<Option<Arc<anyhow::Error>>>,
    /// Deliveries waiting for this cell's turn in the settle in progress, in
    /// arrival order. Empty between settles.
    pub(crate) pending: Mutex<Vec<(Stamp, Signal<T>)>>,
    /// True while this cell is in the settle frontier.
    pub(crate) queued: AtomicBool,
    /// Writes to this cell that a callback queued and no settle has seeded.
    pub(crate) backlog: crate::settle::Backlog,
    /// What a second value does to a pending one. Unset keeps both.
    pub(crate) combine: std::sync::OnceLock<Combine<T>>,
    /// The settle in which this cell last fanned out.
    pub(crate) fired_run: std::sync::atomic::AtomicU64,
    /// Height cache: packed `(height_epoch << 32) | height`. Valid while
    /// `height_epoch` is unchanged; `0` means never computed.
    pub(crate) height_cache: std::sync::atomic::AtomicU64,
    /// Bumped when an edge change in this cell's dependency cone could change
    /// its height. Starts at 1 so a zeroed `height_cache` reads as stale.
    pub(crate) height_epoch: std::sync::atomic::AtomicU64,
    /// Weak back-edges to the cells whose height depends on this one. May hold
    /// a stale entry, which costs one extra recompute, but never misses a live
    /// dependent, which would leave that dependent firing too early.
    pub(crate) dependents: Mutex<Vec<Weak<dyn Node>>>,
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
            owned: Mutex::new(FxHashMap::default()),
            completed: AtomicBool::new(false),
            errored: AtomicBool::new(false),
            error: Mutex::new(None),
            pending: Mutex::new(Vec::new()),
            queued: AtomicBool::new(false),
            backlog: crate::settle::Backlog::default(),
            combine: std::sync::OnceLock::new(),
            fired_run: std::sync::atomic::AtomicU64::new(0),
            height_cache: std::sync::atomic::AtomicU64::new(0),
            height_epoch: std::sync::atomic::AtomicU64::new(1),
            dependents: Mutex::new(Vec::new()),
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
        T: CellValue,
    {
        self.own_keyed(Uuid::new_v4(), guard);
    }

    /// Take ownership of a subscription guard with a stable key.
    ///
    /// If a guard with the same key already exists, it is replaced (and dropped).
    /// This is used by `switch_map` to ensure the old inner subscription is cleaned up
    /// when switching to a new inner cell.
    pub fn own_keyed(&self, key: Uuid, guard: SubscriptionGuard)
    where
        T: CellValue,
    {
        // Register as a dependent of the new source before the guard moves
        // into `owned`. A replaced guard leaves a stale back-edge on its old
        // source, which only over-invalidates.
        self.declare_source(guard.source());
        // A source below this cell cannot raise its height. A replaced guard
        // can only lower it, and a height that is too high still orders this
        // cell after every dependency.
        let raises = crate::settle::height(guard.source().as_ref())
            >= Node::node_height(self.inner.as_ref());
        let previous = self.inner.owned.lock().insert(key, guard);
        drop(previous);
        if raises {
            crate::settle::invalidate_height_cone(self.inner.as_ref());
        }
    }

    /// Declare the edge from `source` to this cell before subscribing to it,
    /// so that no write from `source` reaches this cell without the edge.
    /// [`own_keyed`](Self::own_keyed) declares it too; declaring twice is a
    /// no-op.
    pub(crate) fn declare_source(&self, source: &Arc<dyn DepNode>)
    where
        T: CellValue,
    {
        let erased: Arc<dyn Node> = self.inner.clone();
        source.add_dependent(Arc::downgrade(&erased));
    }

    /// Drop the guard that [`own_keyed`](Self::own_keyed) stored under `key`.
    pub(crate) fn release_owned(&self, key: Uuid) {
        let released = self.inner.owned.lock().remove(&key);
        drop(released);
    }
}

// ============================================================================
// DepNode implementation for Cell - enables type-erased dependency traversal
// ============================================================================

impl<T: CellValue, M> Cell<T, M> {
    /// Make this cell keep only the last value it receives in a settle, so it
    /// fans out once. For the output of a fan-in operator, where each input's
    /// callback emits the full combined state.
    #[must_use]
    pub(crate) fn combine_latest(self) -> Self {
        let _ = self.inner.combine.set(Combine::Latest);
        self
    }

    /// Make this cell fold each value it receives in a settle into the pending
    /// one, so it fans out once with the folded value.
    #[must_use]
    pub(crate) fn combine_with(self, fold: fn(&mut Arc<T>, Arc<T>) -> Option<Arc<T>>) -> Self {
        let _ = self.inner.combine.set(Combine::Fold(fold));
        self
    }

    /// Make this cell deliver every value it receives, including values
    /// written through `Mutable::set`. For a cell that carries events.
    #[must_use]
    pub(crate) fn combine_each(self) -> Self {
        let _ = self.inner.combine.set(Combine::Each);
        self
    }
}

impl<T: CellValue> Node for CellInner<T> {
    fn node_id(&self) -> Uuid {
        self.id
    }

    fn node_height(&self) -> u64 {
        let epoch = u32::try_from(self.height_epoch.load(Ordering::Relaxed)).unwrap_or(u32::MAX);
        let packed = self.height_cache.load(Ordering::Relaxed);
        if u32::try_from(packed >> 32).unwrap_or(u32::MAX) == epoch {
            return packed & 0xFFFF_FFFF;
        }
        let sources: Vec<_> = self
            .owned
            .lock()
            .values()
            .map(|guard| Arc::clone(guard.source()))
            .collect();
        let height = sources.iter().fold(0u64, |height, source| {
            height.max(crate::settle::height(source.as_ref()).saturating_add(1))
        });
        self.height_cache
            .store((u64::from(epoch) << 32) | height, Ordering::Relaxed);
        height
    }

    fn claim(&self) -> bool {
        !self.queued.swap(true, Ordering::AcqRel)
    }

    fn fire(&self) {
        // Cleared before the take: a write that joins while this cell fires
        // must schedule it again, or its value stays pending. The cost is one
        // empty fire when the write lands between the two lines.
        self.queued.store(false, Ordering::Release);
        let pending = std::mem::take(&mut *self.pending.lock());
        let pending = self.in_settle_order(pending);
        for signal in pending {
            if self.completed.load(Ordering::SeqCst) || self.errored.load(Ordering::SeqCst) {
                break;
            }
            self.write_value(&signal);
            crate::settle::record_fire(&self.fired_run);
            #[cfg(debug_assertions)]
            let started = crate::platform::Instant::now();
            self.fanout(&signal);
            // Every writer in the process queues behind this fanout. Name the
            // cell when one takes long enough to look like a hang.
            #[cfg(debug_assertions)]
            if started.elapsed() > SLOW_FANOUT {
                log::warn!(
                    "hyphae: one fanout of the cell created at {} held the settle for {:?}; \
                     a subscriber that blocks or computes for long delays every write in the process",
                    self.caller,
                    started.elapsed()
                );
            }
        }
    }

    fn bump_height_epoch(&self) {
        self.height_epoch.fetch_add(1, Ordering::Relaxed);
    }

    fn node_dependents(&self) -> Vec<Weak<dyn Node>> {
        let mut dependents = self.dependents.lock();
        dependents.retain(|dependent| dependent.strong_count() > 0);
        dependents.clone()
    }

    fn node_caller(&self) -> Option<&'static std::panic::Location<'static>> {
        Some(self.caller)
    }
}

impl<T: CellValue> CellInner<T> {
    /// Route one signal: into the settle in progress when this cell has not
    /// had its turn there, otherwise through the input queue.
    ///
    /// `state` is `true` for `Mutable::set` and `false` for `notify`. Queued
    /// sets of one cell that share a settle leave the last value. A set that
    /// joins a settle keeps its arrival position, because its writer decides
    /// the order of its own state. A notify is an event and takes the
    /// position a one-thread settle would have given it.
    pub(crate) fn write(self: &Arc<Self>, signal: Signal<T>, state: bool) {
        if self.completed.load(Ordering::SeqCst) || self.errored.load(Ordering::SeqCst) {
            return;
        }
        match crate::settle::position(self.as_ref(), &self.backlog) {
            crate::settle::Position::Joins => {
                self.accept(
                    signal,
                    false,
                    if state { Order::Arrival } else { Order::Settle },
                );
            }
            crate::settle::Position::Queue(backlogged) => crate::settle::submit_write(
                crate::settle::SettleKey::Shared,
                Box::new(CellWrite {
                    cell: Arc::downgrade(self),
                    signal,
                    latest: state,
                    _backlogged: backlogged,
                }),
            ),
        }
    }

    /// Add `signal` to this cell's pending list and put the cell in the
    /// frontier. Only a thread that is settling calls this.
    fn accept(self: &Arc<Self>, signal: Signal<T>, latest: bool, order: Order) {
        self.accept_stamped(stamp(), signal, latest, order);
    }

    fn accept_stamped(
        self: &Arc<Self>,
        stamp: Stamp,
        signal: Signal<T>,
        latest: bool,
        order: Order,
    ) {
        let latest = match self.combine.get() {
            Some(Combine::Latest) => true,
            Some(Combine::Each | Combine::Fold(_)) => false,
            None => latest,
        };
        let displaced = {
            let mut pending = self.pending.lock();
            match (pending.last_mut(), signal) {
                // The last arrival wins whatever its stamp: a fan-in pushes
                // under its state lock, so a later push holds more.
                (Some((_, Signal::Value(held))), Signal::Value(next)) if latest => {
                    Some(std::mem::replace(held, next))
                }
                (last, signal) => {
                    // A terminal ends the stream after what its writer has
                    // seen, so it stays behind every value already here.
                    let order = match signal {
                        Signal::Value(_) => order,
                        Signal::Complete | Signal::Error(_) => Order::Arrival,
                    };
                    let stamp = match (order, last) {
                        (Order::Arrival, Some((last, _))) => stamp.max(*last),
                        _ => stamp,
                    };
                    pending.push((stamp, signal));
                    None
                }
            }
        };
        // A replaced value can own cells whose drop takes other cell locks.
        drop(displaced);
        let node: Arc<dyn Node> = self.clone();
        crate::settle::schedule(Arc::downgrade(&node), self.node_height());
    }

    /// Put the pending values in the order a one-thread settle delivers
    /// them, then fold them when this cell folds.
    ///
    /// Nodes of one height fire at the same time, so their values reach this
    /// cell in any order. The stamp restores the order. Values that one node
    /// pushed share a stamp and keep their arrival order. A value pushed with
    /// [`Order::Arrival`] never moves ahead of a value that arrived before it.
    /// A `Latest` cell keeps arrival order.
    fn in_settle_order(&self, mut pending: Vec<(Stamp, Signal<T>)>) -> Vec<Signal<T>> {
        let combine = self.combine.get();
        if !matches!(combine, Some(Combine::Latest))
            && !pending.is_sorted_by_key(|(stamp, _)| *stamp)
        {
            pending.sort_by_key(|(stamp, _)| *stamp);
        }
        let signals = pending.into_iter().map(|(_, signal)| signal);
        let Some(Combine::Fold(fold)) = combine else {
            return signals.collect();
        };
        let mut folded: Vec<Signal<T>> = Vec::new();
        for signal in signals {
            match (folded.last_mut(), signal) {
                (Some(Signal::Value(held)), Signal::Value(next)) => {
                    if let Some(next) = fold(held, next) {
                        folded.push(Signal::Value(next));
                    }
                }
                (_, signal) => folded.push(signal),
            }
        }
        folded
    }

    /// Run `write` at the point this cell takes a value: now when the cell
    /// joins the settle in progress, otherwise when a later settle is seeded.
    ///
    /// For a fan-in that must read its state and push the result as one step
    /// under its state lock. `write` takes that lock itself and calls the
    /// sink while it holds it. A queued write holds no lock while its writer
    /// waits, and reads the state when it is applied, so it cannot carry a
    /// snapshot older than a value already delivered.
    pub(crate) fn write_with(
        self: &Arc<Self>,
        write: impl FnOnce(&mut dyn FnMut(Signal<T>)) + Send + 'static,
    ) {
        if self.completed.load(Ordering::SeqCst) || self.errored.load(Ordering::SeqCst) {
            return;
        }
        match crate::settle::position(self.as_ref(), &self.backlog) {
            crate::settle::Position::Joins => {
                write(&mut |signal| self.accept(signal, false, Order::Arrival));
            }
            crate::settle::Position::Queue(backlogged) => crate::settle::submit_write(
                crate::settle::SettleKey::Shared,
                Box::new(DeferredWrite {
                    cell: Arc::downgrade(self),
                    write,
                    _backlogged: backlogged,
                }),
            ),
        }
    }
}

/// A queued [`CellInner::write_with`].
struct DeferredWrite<T, F> {
    cell: Weak<CellInner<T>>,
    write: F,
    /// Dropped after `apply` has seeded the write.
    _backlogged: Option<crate::settle::Backlogged>,
}

impl<T, F> crate::settle::Write for DeferredWrite<T, F>
where
    T: CellValue,
    F: FnOnce(&mut dyn FnMut(Signal<T>)) + Send + 'static,
{
    fn apply(self: Box<Self>) {
        if let Some(cell) = self.cell.upgrade() {
            (self.write)(&mut |signal| cell.accept(signal, false, Order::Arrival));
        }
    }
}

/// A queued write to a cell. Holds the cell weakly: a cell that nothing else
/// holds has no subscriber left to tell, so its write is dropped.
struct CellWrite<T> {
    cell: Weak<CellInner<T>>,
    signal: Signal<T>,
    latest: bool,
    /// Dropped after `apply` has seeded the write.
    _backlogged: Option<crate::settle::Backlogged>,
}

impl<T: CellValue> crate::settle::Write for CellWrite<T> {
    fn apply(self: Box<Self>) {
        if let Some(cell) = self.cell.upgrade() {
            cell.accept(self.signal, self.latest, Order::Arrival);
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
        let mut sources: Vec<_> = self
            .inner
            .owned
            .lock()
            .values()
            .map(|guard| Arc::clone(guard.source()))
            .collect();
        let mut seen = std::collections::HashSet::new();
        sources.retain(|source| seen.insert(source.id()));
        sources
    }

    fn height_cache(&self) -> Option<&std::sync::atomic::AtomicU64> {
        Some(&self.inner.height_cache)
    }

    fn height_epoch(&self) -> Option<&std::sync::atomic::AtomicU64> {
        Some(&self.inner.height_epoch)
    }

    fn add_dependent(&self, dependent: Weak<dyn Node>) {
        let mut dependents = self.inner.dependents.lock();
        // Prune dead entries, then register only a dependent that is not
        // already here. A `switch_map` whose closure returns a cached cell
        // re-registers the same pair on every rewire; without the duplicate
        // check this list grew without bound.
        dependents.retain(|existing| existing.strong_count() > 0);
        // Compare data addresses: vtable identity is not guaranteed for
        // trait-object weaks coerced at different sites.
        let address = dependent.as_ptr().cast::<()>();
        if dependents
            .iter()
            .any(|existing| existing.as_ptr().cast::<()>() == address)
        {
            return;
        }
        dependents.push(dependent);
    }

    fn subscriber_count(&self) -> usize {
        self.inner
            .subscribers
            .lock()
            .len()
            .saturating_add(self.inner.result_subscribers.lock().len())
    }

    fn owned_count(&self) -> usize {
        self.inner.owned.lock().len()
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
    /// Deliver a signal to this cell: store it and tell every subscriber.
    ///
    /// The delivery goes through the settle core. It is complete when this
    /// returns unless another thread is settling, in which case that thread
    /// completes it. Called from a subscriber callback, it joins the settle in
    /// progress when this cell depends on the cell whose callback is running,
    /// and otherwise runs in a later settle.
    #[doc(hidden)]
    pub fn notify(&self, signal: Signal<T>) {
        self.inner.write(signal, false);
    }

    /// See [`CellInner::write_with`].
    pub(crate) fn write_with(
        &self,
        write: impl FnOnce(&mut dyn FnMut(Signal<T>)) + Send + 'static,
    ) {
        self.inner.write_with(write);
    }
}

impl<T: CellValue> CellInner<T> {
    /// Store this cell's current value, or its completed or errored state.
    #[cfg_attr(feature = "profiling", inline(never))]
    fn write_value(&self, signal: &Signal<T>) {
        match signal {
            Signal::Value(value) => {
                let previous = std::mem::replace(&mut *self.value.lock(), value.clone());
                drop(previous);
            }
            Signal::Complete => {
                self.completed.store(true, Ordering::SeqCst);
            }
            Signal::Error(err) => {
                self.errored.store(true, Ordering::SeqCst);
                *self.error.lock() = Some(err.clone());
            }
        }
    }

    /// Fan `signal` out to this cell's subscribers. Callbacks run with no
    /// internal lock held.
    #[cfg_attr(feature = "profiling", inline(never))]
    fn fanout(&self, signal: &Signal<T>) {
        // One tally per fanout against the active measurement pass, if any.
        #[cfg(feature = "profiling")]
        crate::profiling::record_fire(self.id);

        // A `tracing` span per fanout so span-based profilers (`tracing-flame`,
        // `tracing-tracy`) get one entry per cell emit, tagged with the cell's
        // id and (if set) its name. The consumer attaches the subscriber; when
        // `profiling` is off this compiles to nothing. Later phases nest this
        // under a per-frame span.
        #[cfg(feature = "profiling")]
        let _fanout_span = ::tracing::trace_span!(
            "hyphae.fanout",
            cell.id = %self.id,
            cell.name = { self.name.lock().clone() }.as_deref().unwrap_or(""),
        )
        .entered();

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
            let (subs, old_snapshot) = self.subscribers.lock().snapshot();
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
            let (result_subs, old_snapshot) = self.result_subscribers.lock().snapshot();
            drop(old_snapshot);
            result_subs
        };

        for (subscriber_id, sub) in result_subs.as_slice() {
            if let Err(err) = sub.deliver(signal) {
                log::error!(
                    "hyphae: fallible subscriber {} on cell {} returned error: {}",
                    subscriber_id,
                    self.id,
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
        self.inner.write(Signal::value(value), true);
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

#[cfg(test)]
mod dependents_tests {
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

        let len = source.inner.dependents.lock().len();
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

        let len = source.inner.dependents.lock().len();
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

        let len = source.inner.dependents.lock().len();
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

        let len = shared_inner.inner.dependents.lock().len();
        assert!(
            len <= 2,
            "switch_map re-knit onto a cached inner cell grew dependents to {len}"
        );
    }
}

#[cfg(test)]
mod owned_guard_tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use super::*;

    #[cfg(feature = "profiling")]
    #[test]
    fn disabled_fanout_trace_does_not_acquire_name_lock() {
        let source = Cell::new(0_u64).with_name("trace-test");
        let sender = source.clone();
        let name_lock = source.inner.name.lock();
        let (done, received) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            ::tracing::subscriber::with_default(
                ::tracing::subscriber::NoSubscriber::default(),
                || {
                    sender.inner.fanout(&Signal::value(1));
                },
            );
            let _ = done.send(());
        });
        let delivered = received.recv_timeout(std::time::Duration::from_secs(5));
        drop(name_lock);
        assert!(delivered.is_ok());
        assert!(thread.join().is_ok());
    }

    #[test]
    fn owned_subscription_lives_until_owner_drops() {
        let source = Cell::new(0u64);
        let owner = Cell::new(0u64);
        let weak = owner.downgrade();
        let notifications = Arc::new(AtomicUsize::new(0));
        let observed = notifications.clone();
        owner.own(source.subscribe(move |_| {
            observed.fetch_add(1, Ordering::Relaxed);
        }));
        source.set(1);
        assert_eq!(notifications.load(Ordering::Relaxed), 2);
        assert_eq!(source.subscriber_count(), 1);
        assert_eq!(owner.owned_count(), 1);
        drop(owner);
        assert!(weak.upgrade().is_none());
        assert_eq!(source.subscriber_count(), 0);
        source.set(2);
        assert_eq!(notifications.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn keyed_replacement_unsubscribes_old_source_and_preserves_new_source() {
        let old = Cell::new(0u64);
        let new = Cell::new(0u64);
        let owner = Cell::new(0u64);
        let key = Uuid::new_v4();
        let received = Arc::new(Mutex::new(Vec::new()));
        let sink = received.clone();
        owner.own_keyed(
            key,
            old.subscribe(move |signal| {
                if let Signal::Value(value) = signal {
                    sink.lock().push(**value);
                }
            }),
        );
        let sink = received.clone();
        owner.own_keyed(
            key,
            new.subscribe(move |signal| {
                if let Signal::Value(value) = signal {
                    sink.lock().push(**value);
                }
            }),
        );
        assert_eq!(owner.owned_count(), 1);
        assert_eq!(old.subscriber_count(), 0);
        assert_eq!(new.subscriber_count(), 1);
        old.set(1);
        new.set(2);
        assert_eq!(*received.lock(), vec![0, 0, 2]);
        drop(owner);
        assert_eq!(new.subscriber_count(), 0);
    }

    #[test]
    fn replaced_guard_cleanup_can_reenter_owner() {
        let owner = Cell::new(0u64);
        let weak = owner.downgrade();
        let key = Uuid::new_v4();
        let cleanups = Arc::new(AtomicUsize::new(0));
        let cleanup_count = cleanups.clone();
        owner.own_keyed(
            key,
            SubscriptionGuard::from_callback(move || {
                cleanup_count.fetch_add(1, Ordering::Relaxed);
                if let Some(owner) = weak.upgrade() {
                    assert!(owner.inner.owned.try_lock().is_some());
                    owner.own(SubscriptionGuard::from_callback(|| {}));
                }
            }),
        );
        let cleanup_count = cleanups.clone();
        owner.own_keyed(
            key,
            SubscriptionGuard::from_callback(move || {
                cleanup_count.fetch_add(1, Ordering::Relaxed);
            }),
        );
        assert_eq!(cleanups.load(Ordering::Relaxed), 1);
        assert_eq!(owner.owned_count(), 2);
        drop(owner);
        assert_eq!(cleanups.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn owned_dependencies_are_deduplicated_by_source_id() {
        let source = Cell::new(0u64);
        let other = Cell::new(0u64);
        let owner = Cell::new(0u64);
        owner.own(source.subscribe(|_| {}));
        owner.own(source.subscribe(|_| {}));
        owner.own(other.subscribe(|_| {}));
        assert_eq!(owner.owned_count(), 3);
        let ids: std::collections::HashSet<_> = owner.deps().iter().map(|dep| dep.id()).collect();
        assert_eq!(ids, [source.id(), other.id()].into_iter().collect());
    }

    #[test]
    fn dependency_identity_is_read_outside_owned_lock() {
        struct ReentrantDependency {
            id: Uuid,
            owner: WeakCell<u64, CellMutable>,
        }
        impl DepNode for ReentrantDependency {
            fn id(&self) -> Uuid {
                if let Some(owner) = self.owner.upgrade() {
                    assert!(owner.inner.owned.try_lock().is_some());
                }
                self.id
            }
            fn name(&self) -> Option<String> {
                None
            }
            fn deps(&self) -> Vec<Arc<dyn DepNode>> {
                Vec::new()
            }
        }
        let owner = Cell::new(0u64);
        let id = Uuid::new_v4();
        let dependency = Arc::new(ReentrantDependency {
            id,
            owner: owner.downgrade(),
        });
        owner.own(SubscriptionGuard::new(Uuid::new_v4(), dependency, || {}));
        assert_eq!(owner.deps().first().map(|dep| dep.id()), Some(id));
    }

    #[test]
    fn concurrent_keyed_replacement_drops_every_guard_once() {
        for shared_key in [false, true] {
            let owner = Cell::new(0u64);
            let cleanups = Arc::new(AtomicUsize::new(0));
            let key = Uuid::new_v4();
            let keys: Vec<_> = (0..4)
                .map(|_| if shared_key { key } else { Uuid::new_v4() })
                .collect();
            let retained = if shared_key { 1 } else { 4 };
            std::thread::scope(|scope| {
                for key in &keys {
                    let owner = owner.clone();
                    let cleanups = cleanups.clone();
                    scope.spawn(move || {
                        for _ in 0..100 {
                            let cleanups = cleanups.clone();
                            owner.own_keyed(
                                *key,
                                SubscriptionGuard::from_callback(move || {
                                    cleanups.fetch_add(1, Ordering::Relaxed);
                                }),
                            );
                            assert!(owner.owned_count() <= retained);
                            assert!(owner.deps().len() <= retained);
                        }
                    });
                }
            });
            assert_eq!(owner.owned_count(), retained);
            assert_eq!(cleanups.load(Ordering::Relaxed), 400 - retained);
            drop(owner);
            assert_eq!(cleanups.load(Ordering::Relaxed), 400);
        }
    }
}
