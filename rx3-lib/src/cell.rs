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

use crate::subscription::SubscriptionGuard;
use crate::traits::{DepNode, Gettable, Mutable, Watchable};

#[derive(Debug, Clone)]
pub struct CellMutable;

#[derive(Debug, Clone)]
pub struct CellImmutable;

/// The inner data of a Cell, wrapped in Arc for shared ownership.
pub(crate) struct CellInner<T, M> {
    pub(crate) id: Uuid,
    pub(crate) subscribers: DashMap<Uuid, Box<Subscriber<T>>>,
    pub(crate) value: ArcSwap<T>,
    pub(crate) name: Mutex<Option<Arc<str>>>,
    pub(crate) dependencies: Vec<Arc<dyn DepNode>>,
    /// Owned values that should be dropped when this cell is dropped.
    /// Used to hold SubscriptionGuards so they unsubscribe on cell drop.
    pub(crate) owned: DashMap<Uuid, Box<dyn Send + Sync>>,
    /// Whether this cell has completed (no more values will be emitted).
    pub(crate) completed: AtomicBool,
    /// Callbacks to invoke when this cell completes.
    pub(crate) on_complete: DashMap<Uuid, Box<dyn Fn() + Send + Sync>>,
    pub(crate) _phantom: PhantomData<M>,
}

/// A reactive cell that holds a value and notifies subscribers on change.
pub struct Cell<T, M> {
    pub(crate) inner: Arc<CellInner<T, M>>,
}

/// A weak reference to a Cell that doesn't prevent it from being dropped.
pub struct WeakCell<T, M> {
    inner: Weak<CellInner<T, M>>,
}

impl<T, M> WeakCell<T, M> {
    /// Try to upgrade to a strong Cell reference.
    /// Returns None if the Cell has been dropped.
    pub fn upgrade(&self) -> Option<Cell<T, M>> {
        self.inner.upgrade().map(|inner| Cell { inner })
    }
}

impl<T, M> Clone for WeakCell<T, M> {
    fn clone(&self) -> Self {
        WeakCell {
            inner: self.inner.clone(),
        }
    }
}

pub(crate) struct Subscriber<T> {
    pub(crate) callback: Arc<dyn Fn(&T) + Send + Sync>,
}

impl<T> Subscriber<T> {
    pub(crate) fn new(callback: impl Fn(&T) + Send + Sync + 'static) -> Self {
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
                dependencies: Vec::new(),
                owned: DashMap::new(),
                completed: AtomicBool::new(false),
                on_complete: DashMap::new(),
                _phantom: PhantomData::<CellMutable>,
            }),
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
        }
    }
}

impl<T, M> Cell<T, M> {
    /// Create a weak reference to this cell.
    /// The weak reference doesn't prevent the cell from being dropped.
    pub fn downgrade(&self) -> WeakCell<T, M> {
        WeakCell {
            inner: Arc::downgrade(&self.inner),
        }
    }

    /// Take ownership of a value, dropping it when this cell is dropped.
    /// Used to hold SubscriptionGuards so they unsubscribe automatically.
    pub(crate) fn own(&self, value: impl Send + Sync + 'static) {
        self.inner.owned.insert(Uuid::new_v4(), Box::new(value));
    }

    /// Returns true if this cell has completed (no more values will be emitted).
    pub fn is_complete(&self) -> bool {
        self.inner.completed.load(Ordering::SeqCst)
    }
}

impl<T: Send + Sync + 'static, M: Send + Sync + 'static> Cell<T, M> {
    /// Register a callback to be called when this cell completes.
    /// Returns a guard that unregisters the callback when dropped.
    pub fn on_complete(&self, callback: impl Fn() + Send + Sync + 'static) -> SubscriptionGuard {
        // If already complete, call immediately
        if self.is_complete() {
            callback();
        }

        let id = Uuid::new_v4();
        self.inner.on_complete.insert(id, Box::new(callback));

        let cell = self.clone();
        SubscriptionGuard::new(id, move || {
            cell.inner.on_complete.remove(&id);
        })
    }

    /// Mark this cell as complete. No more values should be emitted after this.
    /// Calls all registered on_complete callbacks.
    pub(crate) fn complete(&self) {
        // Only complete once
        if self.inner.completed.swap(true, Ordering::SeqCst) {
            return;
        }

        // Collect callback ids first to release lock
        let ids: Vec<_> = self
            .inner
            .on_complete
            .iter()
            .map(|entry| *entry.key())
            .collect();

        // Call each callback
        for id in ids {
            if let Some(entry) = self.inner.on_complete.get(&id) {
                let _ = catch_unwind(AssertUnwindSafe(|| {
                    (entry.value())();
                }));
            }
        }
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

    fn deps(&self) -> &[Arc<dyn DepNode>] {
        &self.inner.dependencies
    }
}

impl<T: Clone + Send + Sync + 'static> Cell<T, CellImmutable> {
    /// Creates a derived cell with dependencies. Used internally by `map` and `combine!`.
    #[doc(hidden)]
    pub fn derived(initial: T, deps: Vec<Arc<dyn DepNode>>) -> Self {
        Self {
            inner: Arc::new(CellInner {
                id: Uuid::new_v4(),
                subscribers: DashMap::new(),
                value: ArcSwap::from_pointee(initial),
                name: Mutex::new(None),
                dependencies: deps,
                owned: DashMap::new(),
                completed: AtomicBool::new(false),
                on_complete: DashMap::new(),
                _phantom: PhantomData,
            }),
        }
    }

    pub fn with_name(self, name: impl Into<Arc<str>>) -> Self {
        *self.inner.name.lock().unwrap() = Some(name.into());
        self
    }
}

impl<T: Clone + Send + Sync + 'static, M: Send + Sync + 'static> Cell<T, M> {
    /// Internal: store value and notify subscribers
    #[doc(hidden)]
    pub fn notify(&self, value: T) {
        self.inner.value.store(Arc::new(value.clone()));

        // Collect callbacks first to release DashMap lock before calling them
        let callbacks: Vec<_> = self
            .inner
            .subscribers
            .iter()
            .map(|entry| Arc::clone(&entry.value().callback))
            .collect();

        // Call each callback, catching panics so one bad callback doesn't kill the rest
        for callback in callbacks {
            let _ = catch_unwind(AssertUnwindSafe(|| {
                callback(&value);
            }));
        }
    }
}

impl<T: Clone + Send + Sync + 'static, U: Send + Sync + 'static> Gettable<T> for Cell<T, U> {
    fn get(&self) -> T {
        (**self.inner.value.load()).clone()
    }
}

impl<T: Clone + Send + Sync + 'static, U: Send + Sync + 'static> Watchable<T> for Cell<T, U> {
    fn subscribe(&self, callback: impl Fn(&T) + Send + Sync + 'static) -> SubscriptionGuard {
        callback(&self.get());
        let id = Uuid::new_v4();
        self.inner.subscribers.insert(id, Box::new(Subscriber::new(callback)));

        let cell = self.clone();
        SubscriptionGuard::new(id, move || {
            cell.inner.subscribers.remove(&id);
        })
    }

    fn unsubscribe(&self, id: Uuid) {
        self.inner.subscribers.remove(&id);
    }
}

impl<T: Clone + Send + Sync + 'static> Mutable<T> for Cell<T, CellMutable> {
    fn set(&self, value: T) {
        self.notify(value);
    }
}
