use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use crate::cell::{Cell, CellImmutable};
use super::{Gettable, SubscribeExt, Watchable};

pub trait MergeMapExt<T>: Watchable<T> {
    fn merge_map<U, F>(&self, f: F) -> Cell<U, CellImmutable>
    where
        T: Clone + Send + Sync + 'static,
        U: Clone + Send + Sync + 'static,
        F: Fn(&T) -> Cell<U, CellImmutable> + Send + Sync + 'static,
        Self: Clone + Send + Sync + 'static,
    {
        let first_inner = f(&self.get());
        let cell = Cell::<U, CellImmutable>::derived(first_inner.get(), vec![]);

        // Subscribe to first inner with cleanup
        let weak = cell.downgrade();
        let first_inner_guard = first_inner.subscribe(move |value| {
            if let Some(c) = weak.upgrade() {
                c.notify(value.clone());
            }
        });
        cell.own(first_inner_guard);

        // When outer changes, subscribe to new inner (without unsubscribing from previous)
        // Note: merge_map accumulates subscriptions by design - each inner cell stays subscribed
        let weak_outer = cell.downgrade();
        let f = Arc::new(f);
        let first = Arc::new(AtomicBool::new(true));
        let outer_guard = self.subscribe(move |outer_value| {
            if first.swap(false, Ordering::SeqCst) {
                return;
            }

            let Some(c) = weak_outer.upgrade() else { return };

            let inner = f(outer_value);

            // Subscribe to new inner - these subscriptions accumulate
            // They will be cleaned up when the derived cell drops
            let weak_inner = weak_outer.clone();
            let inner_guard = inner.subscribe(move |value| {
                if let Some(c) = weak_inner.upgrade() {
                    c.notify(value.clone());
                }
            });
            c.own(inner_guard);
        });
        cell.own(outer_guard);

        cell
    }
}

impl<T, W: Watchable<T>> MergeMapExt<T> for W {}
