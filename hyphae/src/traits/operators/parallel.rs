use rayon::prelude::*;

use super::{CellValue, Gettable, Watchable};
use crate::{
    cell::{Cell, CellMutable},
    signal::Signal,
    subscription::SubscriptionGuard,
};

/// A cell that notifies subscribers in parallel using Rayon.
pub struct ParallelCell<T> {
    inner: Cell<T, CellMutable>,
}

impl<T: CellValue> ParallelCell<T> {
    pub fn get(&self) -> T {
        self.inner.get()
    }

    pub fn subscribe(
        &self,
        callback: impl Fn(&Signal<T>) + Send + Sync + 'static,
    ) -> SubscriptionGuard {
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

impl<T: CellValue> ParallelCell<T> {
    /// Notify all subscribers in parallel using Rayon.
    pub fn notify(&self, value: T) {
        let signal = Signal::value(value);
        *self.inner.inner.value.lock().expect("cell value poisoned") =
            signal.arc().unwrap().clone();

        // Brief mutex acquire to clone the subscriber-list Arc, then drop the
        // lock and fan out in parallel — callbacks run lock-free.
        let subs = std::sync::Arc::clone(&*self.inner.inner.subscribers.lock());
        subs.par_iter().for_each(|(_, sub)| {
            (sub.callback)(&signal);
        });
    }
}

pub trait ParallelExt<T>: Watchable<T> {
    /// Convert to a parallel cell that notifies subscribers using Rayon.
    fn parallel(&self) -> ParallelCell<T>
    where
        T: CellValue,
        Self: Clone + Send + Sync + 'static,
    {
        let inner = Cell::<T, CellMutable>::new(self.get());
        let parallel = ParallelCell { inner };

        let weak = parallel.inner.downgrade();
        let guard = self.subscribe(move |signal| {
            if let Some(inner) = weak.upgrade() {
                match signal {
                    Signal::Value(value) => {
                        *inner.inner.value.lock().expect("cell value poisoned") = value.clone();

                        // Brief mutex acquire + Arc::clone snapshot.
                        let subs = std::sync::Arc::clone(&*inner.inner.subscribers.lock());
                        subs.par_iter().for_each(|(_, sub)| {
                            (sub.callback)(signal);
                        });
                    }
                    Signal::Complete => inner.notify(Signal::Complete),
                    Signal::Error(e) => inner.notify(Signal::Error(e.clone())),
                }
            }
        });
        parallel.inner.own(guard);

        parallel
    }
}

impl<T, W: Watchable<T>> ParallelExt<T> for W {}
