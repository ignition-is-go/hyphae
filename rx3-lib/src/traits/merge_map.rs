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
            let weak = cell.downgrade();
            first_inner.watch(move |value| {
                if let Some(c) = weak.upgrade() {
                    c.notify(value.clone());
                }
            });
        }

        // When outer changes, subscribe to new inner (without unsubscribing from previous)
        let weak_outer = cell.downgrade();
        let f = Arc::new(f);
        let first = Arc::new(AtomicBool::new(true));
        self.watch(move |outer_value| {
            // Skip first emission - we already handled it above
            if first.swap(false, Ordering::SeqCst) {
                return;
            }

            // Check if output cell still exists
            let Some(_) = weak_outer.upgrade() else { return };

            let inner = f(outer_value);

            let weak_inner = weak_outer.clone();
            inner.watch(move |value| {
                if let Some(c) = weak_inner.upgrade() {
                    c.notify(value.clone());
                }
            });
        });

        cell
    }
}

impl<T, W: Watchable<T>> MergeMapExt<T> for W {}
