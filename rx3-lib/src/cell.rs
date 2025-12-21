use arc_swap::ArcSwap;
use dashmap::DashMap;
use std::{
    fmt::Debug,
    marker::PhantomData,
    panic::{catch_unwind, AssertUnwindSafe},
    sync::{Arc, Mutex, Weak},
};
use uuid::Uuid;

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
    fn watch(&self, callback: impl Fn(&T) + Send + Sync + 'static) -> Uuid {
        callback(&self.get());
        let id = Uuid::new_v4();
        self.inner.subscribers.insert(id, Box::new(Subscriber::new(callback)));
        id
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
