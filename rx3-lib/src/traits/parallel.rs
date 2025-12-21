use rayon::prelude::*;
use std::sync::Arc;
use crate::cell::{Cell, CellImmutable};
use super::{DepNode, Watchable};

/// A cell that notifies subscribers in parallel using Rayon.
pub struct ParallelCell<T> {
    inner: Cell<T, CellImmutable>,
}

impl<T: Clone + Send + Sync + 'static> ParallelCell<T> {
    pub fn get(&self) -> T {
        self.inner.get()
    }

    pub fn watch(&self, callback: impl Fn(&T) + Send + Sync + 'static) {
        self.inner.watch(callback);
    }
}

impl<T> Clone for ParallelCell<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<T: Clone + Send + Sync + 'static> ParallelCell<T> {
    /// Notify all subscribers in parallel using Rayon.
    pub fn notify(&self, value: T) {
        self.inner.value.store(Arc::new(value.clone()));

        // Collect subscribers and notify in parallel
        let callbacks: Vec<_> = self.inner.subscribers.iter()
            .map(|entry| Arc::clone(&entry.value().callback))
            .collect();

        callbacks.par_iter().for_each(|callback| {
            callback(&value);
        });
    }
}

pub trait ParallelExt<T>: Watchable<T> {
    /// Convert to a parallel cell that notifies subscribers using Rayon.
    fn parallel(&self) -> ParallelCell<T>
    where
        T: Clone + Send + Sync + 'static,
    {
        let parent: Arc<dyn DepNode> = Arc::new(self.clone());
        let inner = Cell::<T, CellImmutable>::derived(self.get(), vec![parent]);
        let parallel = ParallelCell { inner };

        let p = parallel.clone();
        self.watch(move |value| {
            p.notify(value.clone());
        });

        parallel
    }
}

impl<T, W: Watchable<T>> ParallelExt<T> for W {}
