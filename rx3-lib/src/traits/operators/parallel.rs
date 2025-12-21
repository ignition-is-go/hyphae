use rayon::prelude::*;
use std::sync::Arc;
use crate::cell::{Cell, CellMutable};
use crate::subscription::SubscriptionGuard;
use super::{Gettable, Watchable};

/// A cell that notifies subscribers in parallel using Rayon.
pub struct ParallelCell<T> {
    inner: Cell<T, CellMutable>,
}

impl<T: Clone + Send + Sync + 'static> ParallelCell<T> {
    pub fn get(&self) -> T {
        self.inner.get()
    }

    pub fn subscribe(&self, callback: impl Fn(&T) + Send + Sync + 'static) -> SubscriptionGuard {
        self.inner.subscribe(callback)
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
        self.inner.inner.value.store(Arc::new(value.clone()));

        // Collect subscribers and notify in parallel
        let callbacks: Vec<_> = self.inner.inner.subscribers.iter()
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
        Self: Clone + Send + Sync + 'static,
    {
        let inner = Cell::<T, CellMutable>::new(self.get());
        let parallel = ParallelCell { inner };

        let weak = parallel.inner.downgrade();
        let guard = self.subscribe(move |value| {
            if let Some(inner) = weak.upgrade() {
                inner.inner.value.store(Arc::new(value.clone()));

                let callbacks: Vec<_> = inner.inner.subscribers.iter()
                    .map(|entry| Arc::clone(&entry.value().callback))
                    .collect();

                callbacks.par_iter().for_each(|callback| {
                    callback(value);
                });
            }
        });
        parallel.inner.own(guard);

        // Propagate source completion
        let weak = parallel.inner.downgrade();
        let complete_guard = self.on_complete(move || {
            if let Some(inner) = weak.upgrade() {
                inner.complete();
            }
        });
        parallel.inner.own(complete_guard);

        parallel
    }
}

impl<T, W: Watchable<T>> ParallelExt<T> for W {}
