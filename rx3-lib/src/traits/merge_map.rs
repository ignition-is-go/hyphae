use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use crate::cell::{Cell, CellImmutable};
use super::{Gettable, Watchable};

pub trait MergeMapExt<T>: Watchable<T> {
    fn merge_map<U, F>(&self, f: F) -> Cell<U, CellImmutable>
    where
        T: Clone + Send + Sync + 'static,
        U: Clone + Send + Sync + 'static,
        F: Fn(&T) -> Cell<U, CellImmutable> + Send + Sync + 'static,
    {
        let first_inner = f(&self.get());
        let cell = Cell::<U, CellImmutable>::derived(first_inner.get(), vec![]);

        // Subscribe to first inner
        {
            let cell = cell.clone();
            first_inner.watch(move |value| {
                cell.notify(value.clone());
            });
        }

        // When outer changes, subscribe to new inner (without unsubscribing from previous)
        let cell_outer = cell.clone();
        let f = Arc::new(f);
        let first = Arc::new(AtomicBool::new(true));
        self.watch(move |outer_value| {
            // Skip first emission - we already handled it above
            if first.swap(false, Ordering::SeqCst) {
                return;
            }

            let inner = f(outer_value);

            let cell_inner = cell_outer.clone();
            inner.watch(move |value| {
                cell_inner.notify(value.clone());
            });
        });

        cell
    }
}

impl<T, W: Watchable<T>> MergeMapExt<T> for W {}
