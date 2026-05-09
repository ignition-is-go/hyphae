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
    /// Subscriber registry stored as `Mutex<Arc<Vec<…>>>`. Reads (notify) take
    /// the lock briefly to clone the Arc and drop the lock before invoking
    /// callbacks, so user code never runs with an internal cell mutex held.
    /// Writes (subscribe/unsubscribe) take the lock, build the next snapshot,
    /// `mem::replace` the slot, and let the displaced Arc drop *outside* the
    /// lock — the same swap-and-defer invariant that `subscribers_writer +
    /// ArcSwap` previously provided, but without arc-swap's per-mutation
    /// `Debt::pay_all` slot-walk that dominated cell-drop cascades.
    pub(crate) subscribers: parking_lot::Mutex<Arc<Vec<(Uuid, Arc<Subscriber<T>>)>>>,
    /// Fallible subscribers. Invoked after `subscribers` on each notify;
    /// `Err` values are logged via `log::error!` and do not propagate.
    pub(crate) result_subscribers:
        parking_lot::Mutex<Arc<Vec<(Uuid, Arc<ResultSubscriber<T>>)>>>,
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
}

impl<T, M> Clone for WeakCell<T, M> {
    fn clone(&self) -> Self {
        WeakCell {
            inner: self.inner.clone(),
            _marker: PhantomData,
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
            subscribers: parking_lot::Mutex::new(Arc::new(Vec::new())),
            result_subscribers: parking_lot::Mutex::new(Arc::new(Vec::new())),
            value: Mutex::new(Arc::new(initial_value)),
            name: Mutex::new(None),
            owned: DashMap::new(),
            completed: AtomicBool::new(false),
            errored: AtomicBool::new(false),
            error: Mutex::new(None),
            #[cfg(feature = "metrics")]
            metrics: default_metrics(),
            #[cfg(feature = "metrics")]
            slow_subscriber_threshold_ns: ArcSwap::from_pointee(None),
            #[cfg(feature = "metrics")]
            slow_subscriber_callback: ArcSwap::from_pointee(None),
            caller: Location::caller(),
        });
        #[cfg(all(feature = "inspector", not(target_arch = "wasm32")))]
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
            subscribers: parking_lot::Mutex::new(Arc::new(Vec::new())),
            result_subscribers: parking_lot::Mutex::new(Arc::new(Vec::new())),
            value: Mutex::new(Arc::new(initial_value)),
            name: Mutex::new(None),
            owned: DashMap::new(),
            completed: AtomicBool::new(false),
            errored: AtomicBool::new(false),
            error: Mutex::new(None),
            metrics: Some(Arc::new(CellMetrics::new())),
            slow_subscriber_threshold_ns: ArcSwap::from_pointee(None),
            slow_subscriber_callback: ArcSwap::from_pointee(None),
            caller: Location::caller(),
        });
        #[cfg(all(feature = "inspector", not(target_arch = "wasm32")))]
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
        #[cfg(all(feature = "inspector", not(target_arch = "wasm32")))]
        crate::registry::registry().mark_owned(guard.source().id(), self.inner.id);
        self.inner.owned.insert(Uuid::new_v4(), guard);
        #[cfg(feature = "trace")]
        crate::tracing::update_owned_count(self.inner.id, self.inner.owned.len());
    }

    /// Take ownership of a subscription guard with a stable key.
    ///
    /// If a guard with the same key already exists, it is replaced (and dropped).
    /// This is used by `switch_map` to ensure the old inner subscription is cleaned up
    /// when switching to a new inner cell.
    pub fn own_keyed(&self, key: Uuid, guard: SubscriptionGuard) {
        #[cfg(all(feature = "inspector", not(target_arch = "wasm32")))]
        {
            // Unmark old owned cell if being replaced
            if let Some((_, old_guard)) = self.inner.owned.remove(&key) {
                crate::registry::registry().unmark_owned(old_guard.source().id());
            }
            crate::registry::registry().mark_owned(guard.source().id(), self.inner.id);
        }
        self.inner.owned.insert(key, guard);
        #[cfg(feature = "trace")]
        crate::tracing::update_owned_count(self.inner.id, self.inner.owned.len());
    }
}

// ============================================================================
// DepNode implementation for Cell - enables type-erased dependency traversal
// ============================================================================

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

    fn subscriber_count(&self) -> usize {
        let subs = Arc::clone(&*self.inner.subscribers.lock());
        let result_subs = Arc::clone(&*self.inner.result_subscribers.lock());
        subs.len() + result_subs.len()
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
    #[doc(hidden)]
    pub fn notify(&self, signal: Signal<T>) {
        // Don't emit anything after completion or error
        if self.inner.completed.load(Ordering::SeqCst) || self.inner.errored.load(Ordering::SeqCst)
        {
            return;
        }

        // Start timing if metrics enabled
        #[cfg(feature = "metrics")]
        let notify_start = self
            .inner
            .metrics
            .as_ref()
            .map(|_| std::time::Instant::now());

        match &signal {
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

        // Hot path: take the subscribers mutex briefly to clone the Arc<Vec>,
        // drop the lock, then iterate the snapshot with no internal lock held.
        // Subscriber callbacks run lock-free; subscribers added during this
        // iteration land in the next notify's snapshot (same semantics as the
        // previous ArcSwap-based load).
        let subs = Arc::clone(&*self.inner.subscribers.lock());

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
        for (_subscriber_id, sub) in subs.iter() {
            #[cfg(feature = "metrics")]
            let sub_start = metrics.as_ref().map(|_| std::time::Instant::now());

            (sub.callback)(&signal);

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
        let result_subs = Arc::clone(&*self.inner.result_subscribers.lock());

        for (subscriber_id, sub) in result_subs.iter() {
            #[cfg(feature = "metrics")]
            let sub_start = metrics.as_ref().map(|_| std::time::Instant::now());

            if let Err(err) = (sub.callback)(&signal) {
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
                subs.len() + result_subs.len(),
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
        // Send current value immediately (Arc clone, no deep copy)
        let current = self
            .inner
            .value
            .lock()
            .expect("cell value poisoned")
            .clone();
        callback(&Signal::Value(current));

        // If already complete or errored, send that signal too
        if self.is_complete() {
            callback(&Signal::Complete);
        } else if self.is_error()
            && let Some(err) = self.error()
        {
            callback(&Signal::Error(err));
        }

        let id = Uuid::new_v4();
        let sub = Arc::new(Subscriber::new(callback));
        // Swap-and-defer: build the next snapshot under the lock, swap it in
        // with `mem::replace`, and let the displaced `Arc<Vec<…>>` drop AFTER
        // the lock guard does. The old vec's drop can recursively touch this
        // cell's drop chain (Subscribers capture upstream cell handles that
        // may be the only owner), and user-visible Drops must not run while
        // any internal cell mutex is held — that allowed two cells dropping
        // concurrently to acquire each other's mutex and deadlock.
        let _old = {
            let mut guard = self.inner.subscribers.lock();
            let mut next: Vec<(Uuid, Arc<Subscriber<T>>)> = (**guard).clone();
            next.push((id, sub));
            std::mem::replace(&mut *guard, Arc::new(next))
        };

        // Record subscriber added if metrics enabled
        #[cfg(feature = "metrics")]
        if let Some(metrics) = &self.inner.metrics {
            metrics.record_subscriber_added();
        }
        #[cfg(feature = "trace")]
        {
            let subs_len = Arc::clone(&*self.inner.subscribers.lock()).len();
            let result_len = Arc::clone(&*self.inner.result_subscribers.lock()).len();
            crate::tracing::update_subscriber_count(self.inner.id, subs_len + result_len);
        }

        let source: Arc<dyn DepNode> = Arc::new(self.clone());
        let cell = self.clone();
        #[cfg(feature = "metrics")]
        let metrics = self.inner.metrics.clone();
        SubscriptionGuard::new(id, source, move || {
            // Swap-and-defer: see subscribe() above.
            let (removed, _old) = {
                let mut guard = cell.inner.subscribers.lock();
                let prev_len = guard.len();
                let next: Vec<(Uuid, Arc<Subscriber<T>>)> = (**guard)
                    .iter()
                    .filter(|(i, _)| *i != id)
                    .cloned()
                    .collect();
                if next.len() != prev_len {
                    (true, Some(std::mem::replace(&mut *guard, Arc::new(next))))
                } else {
                    (false, None)
                }
            };
            #[cfg(feature = "metrics")]
            if removed && let Some(m) = &metrics {
                m.record_subscriber_removed();
            }
            #[cfg(not(feature = "metrics"))]
            let _ = removed;
            #[cfg(feature = "trace")]
            {
                let subs_len = Arc::clone(&*cell.inner.subscribers.lock()).len();
                let result_len = Arc::clone(&*cell.inner.result_subscribers.lock()).len();
                crate::tracing::update_subscriber_count(cell.inner.id, subs_len + result_len);
            }
        })
    }

    fn unsubscribe(&self, id: Uuid) {
        // Swap-and-defer: see subscribe() — the displaced `Arc<Vec<…>>`s
        // (`_old_subs`, `_old_result_subs`) drop AFTER each lock guard is
        // released so cascading Subscriber/Cell Drops never run with an
        // internal cell mutex held.
        let (removed_from_subs, _old_subs) = {
            let mut guard = self.inner.subscribers.lock();
            let prev_len = guard.len();
            let next: Vec<(Uuid, Arc<Subscriber<T>>)> = (**guard)
                .iter()
                .filter(|(i, _)| *i != id)
                .cloned()
                .collect();
            if next.len() != prev_len {
                (true, Some(std::mem::replace(&mut *guard, Arc::new(next))))
            } else {
                (false, None)
            }
        };
        let (removed_from_result, _old_result_subs) = if removed_from_subs {
            (false, None)
        } else {
            let mut guard = self.inner.result_subscribers.lock();
            let prev_len = guard.len();
            let next: Vec<(Uuid, Arc<ResultSubscriber<T>>)> = (**guard)
                .iter()
                .filter(|(i, _)| *i != id)
                .cloned()
                .collect();
            if next.len() != prev_len {
                (true, Some(std::mem::replace(&mut *guard, Arc::new(next))))
            } else {
                (false, None)
            }
        };
        if removed_from_subs || removed_from_result {
            // Record subscriber removed if metrics enabled
            #[cfg(feature = "metrics")]
            if let Some(metrics) = &self.inner.metrics {
                metrics.record_subscriber_removed();
            }
            #[cfg(feature = "trace")]
            {
                let subs_len = Arc::clone(&*self.inner.subscribers.lock()).len();
                let result_len = Arc::clone(&*self.inner.result_subscribers.lock()).len();
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
        // Swap-and-defer: see Watchable::subscribe above.
        let _old = {
            let mut guard = self.inner.result_subscribers.lock();
            let mut next: Vec<(Uuid, Arc<ResultSubscriber<T>>)> = (**guard).clone();
            next.push((id, sub));
            std::mem::replace(&mut *guard, Arc::new(next))
        };

        #[cfg(feature = "metrics")]
        if let Some(metrics) = &self.inner.metrics {
            metrics.record_subscriber_added();
        }
        #[cfg(feature = "trace")]
        {
            let subs_len = Arc::clone(&*self.inner.subscribers.lock()).len();
            let result_len = Arc::clone(&*self.inner.result_subscribers.lock()).len();
            crate::tracing::update_subscriber_count(self.inner.id, subs_len + result_len);
        }

        let source: Arc<dyn DepNode> = Arc::new(self.clone());
        let cell = self.clone();
        #[cfg(feature = "metrics")]
        let metrics = self.inner.metrics.clone();
        SubscriptionGuard::new(id, source, move || {
            // Swap-and-defer: see Watchable::subscribe above.
            let (removed, _old) = {
                let mut guard = cell.inner.result_subscribers.lock();
                let prev_len = guard.len();
                let next: Vec<(Uuid, Arc<ResultSubscriber<T>>)> = (**guard)
                    .iter()
                    .filter(|(i, _)| *i != id)
                    .cloned()
                    .collect();
                if next.len() != prev_len {
                    (true, Some(std::mem::replace(&mut *guard, Arc::new(next))))
                } else {
                    (false, None)
                }
            };
            #[cfg(feature = "metrics")]
            if removed && let Some(m) = &metrics {
                m.record_subscriber_removed();
            }
            #[cfg(not(feature = "metrics"))]
            let _ = removed;
            #[cfg(feature = "trace")]
            {
                let subs_len = Arc::clone(&*cell.inner.subscribers.lock()).len();
                let result_len = Arc::clone(&*cell.inner.result_subscribers.lock()).len();
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

#[cfg(all(feature = "inspector", not(target_arch = "wasm32")))]
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
        let subs = Arc::clone(&*self.subscribers.lock());
        let result_subs = Arc::clone(&*self.result_subscribers.lock());
        subs.len() + result_subs.len()
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
        #[cfg(all(feature = "inspector", not(target_arch = "wasm32")))]
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
