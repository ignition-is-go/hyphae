use std::{
    fmt::Debug,
    marker::PhantomData,
    panic::{AssertUnwindSafe, Location, catch_unwind},
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use arc_swap::ArcSwap;
use dashmap::DashMap;
use uuid::Uuid;

use crate::{
    metrics::CellMetrics,
    signal::Signal,
    subscription::SubscriptionGuard,
    traits::{CellValue, DepNode, Gettable, Mutable, Watchable},
};

/// Information about a slow subscriber callback.
#[derive(Debug, Clone)]
pub struct SlowSubscriberAlert {
    /// The subscriber ID.
    pub subscriber_id: Uuid,
    /// How long the subscriber took (nanoseconds).
    pub duration_ns: u64,
    /// The configured threshold (nanoseconds).
    pub threshold_ns: u64,
}

type SlowSubscriberCallback = Arc<dyn Fn(SlowSubscriberAlert) + Send + Sync>;

#[derive(Debug, Clone)]
pub struct CellMutable;

#[derive(Debug, Clone)]
pub struct CellImmutable;

/// The inner data of a Cell, wrapped in Arc for shared ownership.
pub(crate) struct CellInner<T> {
    pub(crate) id: Uuid,
    pub(crate) subscribers: DashMap<Uuid, Box<Subscriber<T>>>,
    pub(crate) value: ArcSwap<T>,
    pub(crate) name: ArcSwap<Option<Arc<str>>>,
    /// Subscription guards owned by this cell (dropped when cell drops, provides dependency tracking).
    pub(crate) owned: DashMap<Uuid, SubscriptionGuard>,
    /// Whether this cell has completed (no more values will be emitted).
    pub(crate) completed: AtomicBool,
    /// Whether this cell has errored.
    pub(crate) errored: AtomicBool,
    /// The error, if any.
    pub(crate) error: ArcSwap<Option<Arc<anyhow::Error>>>,
    /// Optional metrics for observability.
    pub(crate) metrics: Option<Arc<CellMetrics>>,
    /// Slow subscriber threshold (nanoseconds). None = disabled.
    pub(crate) slow_subscriber_threshold_ns: ArcSwap<Option<u64>>,
    /// Callback for slow subscriber alerts.
    pub(crate) slow_subscriber_callback: ArcSwap<Option<SlowSubscriberCallback>>,
    /// Source location where this cell was created (via #[track_caller]).
    pub(crate) caller: &'static Location<'static>,
}

/// A reactive cell that holds a value and notifies subscribers on change.
pub struct Cell<T, M> {
    pub(crate) inner: Arc<CellInner<T>>,
    _marker: PhantomData<M>,
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

impl<T: CellValue> Cell<T, CellMutable> {
    #[track_caller]
    pub fn new(initial_value: T) -> Self {
        let inner = Arc::new(CellInner {
            id: Uuid::new_v4(),
            subscribers: DashMap::new(),
            value: ArcSwap::from_pointee(initial_value),
            name: ArcSwap::from_pointee(None),
            owned: DashMap::new(),
            completed: AtomicBool::new(false),
            errored: AtomicBool::new(false),
            error: ArcSwap::from_pointee(None),
            metrics: None,
            slow_subscriber_threshold_ns: ArcSwap::from_pointee(None),
            slow_subscriber_callback: ArcSwap::from_pointee(None),
            caller: Location::caller(),
        });
        #[cfg(all(feature = "inspector", not(target_arch = "wasm32")))]
        crate::registry::registry().register(inner.id, Arc::downgrade(&inner) as Weak<dyn DepNode>);
        Self {
            inner,
            _marker: PhantomData,
        }
    }

    /// Create a new mutable cell with metrics collection enabled.
    #[track_caller]
    pub fn with_metrics(initial_value: T) -> Self {
        let inner = Arc::new(CellInner {
            id: Uuid::new_v4(),
            subscribers: DashMap::new(),
            value: ArcSwap::from_pointee(initial_value),
            name: ArcSwap::from_pointee(None),
            owned: DashMap::new(),
            completed: AtomicBool::new(false),
            errored: AtomicBool::new(false),
            error: ArcSwap::from_pointee(None),
            metrics: Some(Arc::new(CellMetrics::new())),
            slow_subscriber_threshold_ns: ArcSwap::from_pointee(None),
            slow_subscriber_callback: ArcSwap::from_pointee(None),
            caller: Location::caller(),
        });
        #[cfg(all(feature = "inspector", not(target_arch = "wasm32")))]
        crate::registry::registry().register(inner.id, Arc::downgrade(&inner) as Weak<dyn DepNode>);
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
    /// use hypha::{Cell, Mutable};
    /// use std::time::Duration;
    ///
    /// let cell = Cell::with_metrics(0);
    /// cell.on_slow_subscriber(Duration::from_millis(10), |alert| {
    ///     eprintln!("Slow subscriber {:?} took {}ms",
    ///         alert.subscriber_id,
    ///         alert.duration_ns / 1_000_000);
    /// });
    /// ```
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
        self.inner.name.store(Arc::new(Some(name.into())));
        self
    }

    /// Check if the cell appears backed up based on last notify time.
    ///
    /// Returns true if metrics are enabled and the last notify took longer
    /// than 1ms (the default threshold). Use `is_backed_up_threshold()` for
    /// a custom threshold.
    ///
    /// Returns false if metrics are not enabled.
    pub fn is_backed_up(&self) -> bool {
        self.is_backed_up_threshold(std::time::Duration::from_millis(1))
    }

    /// Check if the cell is backed up with a custom threshold.
    ///
    /// Returns true if metrics are enabled and the last notify duration
    /// exceeded the given threshold.
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
    pub fn metrics(&self) -> Option<&CellMetrics> {
        self.inner.metrics.as_ref().map(|m| m.as_ref())
    }

    /// Take ownership of a subscription guard, dropping it when this cell is dropped.
    pub fn own(&self, guard: SubscriptionGuard) {
        #[cfg(all(feature = "inspector", not(target_arch = "wasm32")))]
        crate::registry::registry().mark_owned(guard.source().id(), self.inner.id);
        self.inner.owned.insert(Uuid::new_v4(), guard);
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
        (**self.inner.name.load()).as_ref().map(|s| s.to_string())
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
        self.inner.subscribers.len()
    }

    fn owned_count(&self) -> usize {
        self.inner.owned.len()
    }
}

impl<T: CellValue> Cell<T, CellImmutable> {
    pub fn with_name(self, name: impl Into<Arc<str>>) -> Self {
        self.inner.name.store(Arc::new(Some(name.into())));
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
        let notify_start = self
            .inner
            .metrics
            .as_ref()
            .map(|_| std::time::Instant::now());

        match &signal {
            Signal::Value(arc_value) => {
                // Arc clone = refcount bump, no deep copy
                self.inner.value.store(arc_value.clone());
            }
            Signal::Complete => {
                self.inner.completed.store(true, Ordering::SeqCst);
            }
            Signal::Error(err) => {
                self.inner.errored.store(true, Ordering::SeqCst);
                self.inner.error.store(Arc::new(Some(err.clone())));
            }
        }

        // Collect callbacks with IDs first to release DashMap lock before calling them
        let callbacks: Vec<_> = self
            .inner
            .subscribers
            .iter()
            .map(|entry| (*entry.key(), Arc::clone(&entry.value().callback)))
            .collect();

        // Load slow subscriber config once
        let slow_threshold = **self.inner.slow_subscriber_threshold_ns.load();
        let slow_callback = (**self.inner.slow_subscriber_callback.load()).clone();

        // Call each callback, catching panics so one bad callback doesn't kill the rest
        for (subscriber_id, callback) in callbacks {
            let sub_start = self
                .inner
                .metrics
                .as_ref()
                .map(|_| std::time::Instant::now());

            let _ = catch_unwind(AssertUnwindSafe(|| {
                callback(&signal);
            }));

            // Record subscriber timing and check for slow subscribers
            if let (Some(metrics), Some(start)) = (&self.inner.metrics, sub_start) {
                let elapsed = start.elapsed().as_nanos() as u64;
                metrics.update_slowest_subscriber(elapsed);

                // Check slow subscriber threshold
                if let (Some(threshold), Some(callback)) = (&slow_threshold, &slow_callback)
                    && elapsed > *threshold
                {
                    let alert = SlowSubscriberAlert {
                        subscriber_id,
                        duration_ns: elapsed,
                        threshold_ns: *threshold,
                    };
                    // Catch panics in the alert callback too
                    let _ = catch_unwind(AssertUnwindSafe(|| {
                        callback(alert);
                    }));
                }
            }
        }

        // Record overall notify timing
        if let (Some(metrics), Some(start)) = (&self.inner.metrics, notify_start) {
            metrics.record_notify(start.elapsed().as_nanos() as u64);
        }
    }
}

impl<T: CellValue, U: Send + Sync + 'static> Gettable<T> for Cell<T, U> {
    fn get(&self) -> T {
        (**self.inner.value.load()).clone()
    }
}

impl<T: CellValue, U: Send + Sync + 'static> Watchable<T> for Cell<T, U> {
    fn subscribe(
        &self,
        callback: impl Fn(&Signal<T>) + Send + Sync + 'static,
    ) -> SubscriptionGuard {
        // Send current value immediately (Arc clone, no deep copy)
        callback(&Signal::Value(self.inner.value.load_full()));

        // If already complete or errored, send that signal too
        if self.is_complete() {
            callback(&Signal::Complete);
        } else if self.is_error()
            && let Some(err) = self.error()
        {
            callback(&Signal::Error(err));
        }

        let id = Uuid::new_v4();
        self.inner
            .subscribers
            .insert(id, Box::new(Subscriber::new(callback)));

        // Record subscriber added if metrics enabled
        if let Some(metrics) = &self.inner.metrics {
            metrics.record_subscriber_added();
        }

        let source: Arc<dyn DepNode> = Arc::new(self.clone());
        let cell = self.clone();
        let metrics = self.inner.metrics.clone();
        SubscriptionGuard::new(id, source, move || {
            cell.inner.subscribers.remove(&id);
            // Record subscriber removed if metrics enabled
            if let Some(m) = &metrics {
                m.record_subscriber_removed();
            }
        })
    }

    fn unsubscribe(&self, id: Uuid) {
        if self.inner.subscribers.remove(&id).is_some() {
            // Record subscriber removed if metrics enabled
            if let Some(metrics) = &self.inner.metrics {
                metrics.record_subscriber_removed();
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
        (**self.inner.error.load()).clone()
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
        (**self.name.load()).as_ref().map(|s| s.to_string())
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
        self.subscribers.len()
    }

    fn owned_count(&self) -> usize {
        self.owned.len()
    }

    fn value_debug(&self) -> Option<String> {
        Some(format!("{:?}", &**self.value.load()))
    }

    fn caller(&self) -> Option<&'static Location<'static>> {
        Some(self.caller)
    }
}

#[cfg(all(feature = "inspector", not(target_arch = "wasm32")))]
impl<T> Drop for CellInner<T> {
    fn drop(&mut self) {
        crate::registry::registry().deregister(&self.id);
    }
}
