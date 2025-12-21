use arc_swap::ArcSwap;
use dashmap::DashMap;
use std::{
    fmt::Debug,
    marker::PhantomData,
    sync::{Arc, Mutex},
};
use uuid::Uuid;

use crate::traits::{DepNode, Gettable, Mutable, Watchable};

#[derive(Debug, Clone)]
pub struct CellMutable;

#[derive(Debug, Clone)]
pub struct CellImmutable;

pub struct Cell<T, M> {
    pub(crate) id: Uuid,
    pub(crate) subscribers: Arc<DashMap<Uuid, Box<Subscriber<T>>>>,
    pub(crate) value: Arc<ArcSwap<T>>,
    pub(crate) name: Arc<Mutex<Option<Arc<str>>>>,
    pub(crate) dependencies: Arc<Vec<Arc<dyn DepNode>>>,
    _phantom: PhantomData<M>,
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
            id: Uuid::new_v4(),
            subscribers: Arc::new(DashMap::new()),
            value: Arc::new(ArcSwap::from_pointee(initial_value)),
            name: Arc::new(Mutex::new(None)),
            dependencies: Arc::new(Vec::new()),
            _phantom: PhantomData::<CellMutable>,
        }
    }

    pub fn with_name(self, name: impl Into<Arc<str>>) -> Self {
        *self.name.lock().unwrap() = Some(name.into());
        self
    }

}

impl<T, M> Clone for Cell<T, M> {
    fn clone(&self) -> Self {
        Cell {
            id: self.id,
            subscribers: Arc::clone(&self.subscribers),
            value: Arc::clone(&self.value),
            name: Arc::clone(&self.name),
            dependencies: Arc::clone(&self.dependencies),
            _phantom: PhantomData,
        }
    }
}

// ============================================================================
// DepNode implementation for Cell - enables type-erased dependency traversal
// ============================================================================

impl<T: Send + Sync, M: Send + Sync> DepNode for Cell<T, M> {
    fn id(&self) -> Uuid {
        self.id
    }

    fn name(&self) -> Option<String> {
        self.name.lock().unwrap().as_ref().map(|s| s.to_string())
    }

    fn deps(&self) -> &[Arc<dyn DepNode>] {
        &self.dependencies
    }
}


impl<T: Clone + Send + Sync + 'static> Cell<T, CellImmutable> {
    /// Creates a derived cell with dependencies. Used internally by `map` and `combine!`.
    #[doc(hidden)]
    pub fn derived(initial: T, deps: Vec<Arc<dyn DepNode>>) -> Self {
        Self {
            id: Uuid::new_v4(),
            subscribers: Arc::new(DashMap::new()),
            value: Arc::new(ArcSwap::from_pointee(initial)),
            name: Arc::new(Mutex::new(None)),
            dependencies: Arc::new(deps),
            _phantom: PhantomData,
        }
    }

    pub fn with_name(self, name: impl Into<Arc<str>>) -> Self {
        *self.name.lock().unwrap() = Some(name.into());
        self
    }
}

impl<T: Clone + Send + Sync + 'static, M: Send + Sync + 'static> Cell<T, M> {
    /// Internal: store value and notify subscribers
    #[doc(hidden)]
    pub fn notify(&self, value: T) {
        self.value.store(Arc::new(value.clone()));
        self.subscribers.iter().for_each(|subscriber| {
            (subscriber.callback)(&value);
        });
    }
}

impl<T: Clone + Send + Sync + 'static, U: Send + Sync + 'static> Gettable<T> for Cell<T, U> {
    fn get(&self) -> T {
        (**self.value.load()).clone()
    }
}

impl<T: Clone + Send + Sync + 'static, U: Send + Sync + 'static> Watchable<T> for Cell<T, U> {
    fn watch(&self, callback: impl Fn(&T) + Send + Sync + 'static) -> Uuid {
        callback(&self.get());
        let id = Uuid::new_v4();
        self.subscribers.insert(id, Box::new(Subscriber::new(callback)));
        id
    }

    fn unsubscribe(&self, id: Uuid) {
        self.subscribers.remove(&id);
    }
}

impl<T: Clone + Send + Sync + 'static> Mutable<T> for Cell<T, CellMutable> {
    fn set(&self, value: T) {
        self.notify(value);
    }
}
