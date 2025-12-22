use arc_swap::ArcSwap;
use dashmap::DashMap;
use std::{
    fmt::Debug,
    marker::PhantomData,
    panic::{catch_unwind, AssertUnwindSafe},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, Weak,
    },
};
use uuid::Uuid;

use crate::metrics::CellMetrics;
use crate::signal::Signal;
use crate::subscription::SubscriptionGuard;
use crate::traits::{DepNode, Gettable, Mutable, Watchable};

#[derive(Debug, Clone)]
pub struct CellMutable;

#[derive(Debug, Clone)]
pub struct CellImmutable;

/// The inner data of a Cell, wrapped in Arc for shared ownership.
pub(crate) struct CellInner<T> {
    pub(crate) id: Uuid,
    pub(crate) subscribers: DashMap<Uuid, Box<Subscriber<T>>>,
    pub(crate) value: ArcSwap<T>,
    pub(crate) name: Mutex<Option<Arc<str>>>,
    /// Subscription guards owned by this cell (dropped when cell drops, provides dependency tracking).
    pub(crate) owned: DashMap<Uuid, SubscriptionGuard>,
    /// Whether this cell has completed (no more values will be emitted).
    pub(crate) completed: AtomicBool,
    /// Whether this cell has errored.
    pub(crate) errored: AtomicBool,
    /// The error, if any.
    pub(crate) error: Mutex<Option<Arc<anyhow::Error>>>,
    /// Optional metrics for observability.
    pub(crate) metrics: Option<Arc<CellMetrics>>,
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
        self.inner.upgrade().map(|inner| Cell { inner, _marker: PhantomData })
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

pub(crate) struct Subscriber<T> {
    pub(crate) callback: Arc<dyn Fn(&Signal<T>) + Send + Sync>,
}

impl<T> Subscriber<T> {
    pub(crate) fn new(callback: impl Fn(&Signal<T>) + Send + Sync + 'static) -> Self {
        Self {
            callback: Arc::new(callback),
        }
    }
}

impl<T: Clone + Send + Sync + 'static> Cell<T, CellMutable> {
    pub fn new(initial_value: T) -> Self {
        Self {
            inner: Arc::new(CellInner {
                id: Uuid::new_v4(),
                subscribers: DashMap::new(),
                value: ArcSwap::from_pointee(initial_value),
                name: Mutex::new(None),
                owned: DashMap::new(),
                completed: AtomicBool::new(false),
                errored: AtomicBool::new(false),
                error: Mutex::new(None),
                metrics: None,
            }),
            _marker: PhantomData,
        }
    }

    /// Create a new mutable cell with metrics collection enabled.
    pub fn with_metrics(initial_value: T) -> Self {
        Self {
            inner: Arc::new(CellInner {
                id: Uuid::new_v4(),
                subscribers: DashMap::new(),
                value: ArcSwap::from_pointee(initial_value),
                name: Mutex::new(None),
                owned: DashMap::new(),
                completed: AtomicBool::new(false),
                errored: AtomicBool::new(false),
                error: Mutex::new(None),
                metrics: Some(Arc::new(CellMetrics::new())),
            }),
            _marker: PhantomData,
        }
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
        *self.inner.name.lock().unwrap() = Some(name.into());
        self
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
    pub(crate) fn own(&self, guard: SubscriptionGuard) {
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
        self.inner.name.lock().unwrap().as_ref().map(|s| s.to_string())
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
}

impl<T: Clone + Send + Sync + 'static> Cell<T, CellImmutable> {
    pub fn with_name(self, name: impl Into<Arc<str>>) -> Self {
        *self.inner.name.lock().unwrap() = Some(name.into());
        self
    }
}

impl<T: Clone + Send + Sync + 'static, M: Send + Sync + 'static> Cell<T, M> {
    /// Emit a signal to all subscribers.
    ///
    /// This is the unified notification mechanism for values, completion, and errors.
    #[doc(hidden)]
    pub fn notify(&self, signal: Signal<T>) {
        // Don't emit anything after completion or error
        if self.inner.completed.load(Ordering::SeqCst) || self.inner.errored.load(Ordering::SeqCst) {
            return;
        }

        // Start timing if metrics enabled
        let notify_start = self.inner.metrics.as_ref().map(|_| std::time::Instant::now());

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
                *self.inner.error.lock().unwrap() = Some(err.clone());
            }
        }

        // Collect callbacks first to release DashMap lock before calling them
        let callbacks: Vec<_> = self
            .inner
            .subscribers
            .iter()
            .map(|entry| Arc::clone(&entry.value().callback))
            .collect();

        // Call each callback, catching panics so one bad callback doesn't kill the rest
        for callback in callbacks {
            let sub_start = self.inner.metrics.as_ref().map(|_| std::time::Instant::now());

            let _ = catch_unwind(AssertUnwindSafe(|| {
                callback(&signal);
            }));

            // Record subscriber timing
            if let (Some(metrics), Some(start)) = (&self.inner.metrics, sub_start) {
                let elapsed = start.elapsed().as_nanos() as u64;
                metrics.update_slowest_subscriber(elapsed);
            }
        }

        // Record overall notify timing
        if let (Some(metrics), Some(start)) = (&self.inner.metrics, notify_start) {
            metrics.record_notify(start.elapsed().as_nanos() as u64);
        }
    }
}

impl<T: Clone + Send + Sync + 'static, U: Send + Sync + 'static> Gettable<T> for Cell<T, U> {
    fn get(&self) -> T {
        (**self.inner.value.load()).clone()
    }
}

impl<T: Clone + Send + Sync + 'static, U: Send + Sync + 'static> Watchable<T> for Cell<T, U> {
    fn subscribe(&self, callback: impl Fn(&Signal<T>) + Send + Sync + 'static) -> SubscriptionGuard {
        // Send current value immediately (Arc clone, no deep copy)
        callback(&Signal::Value(self.inner.value.load_full()));

        // If already complete or errored, send that signal too
        if self.is_complete() {
            callback(&Signal::Complete);
        } else if self.is_error() {
            if let Some(err) = self.error() {
                callback(&Signal::Error(err));
            }
        }

        let id = Uuid::new_v4();
        self.inner.subscribers.insert(id, Box::new(Subscriber::new(callback)));

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
        self.inner.error.lock().unwrap().clone()
    }
}

impl<T: Clone + Send + Sync + 'static> Mutable<T> for Cell<T, CellMutable> {
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
