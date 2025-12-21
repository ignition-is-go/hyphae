use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use crate::cell::{Cell, CellImmutable, CellMutable};
use super::{Gettable, Watchable};

pub trait MergeMapExt<T>: Watchable<T> {
    fn merge_map<U, F>(&self, f: F) -> Cell<U, CellImmutable>
    where
        T: Clone + Send + Sync + 'static,
        U: Clone + Send + Sync + 'static,
        F: Fn(&T) -> Cell<U, CellImmutable> + Send + Sync + 'static,
        Self: Clone + Send + Sync + 'static,
    {
        let first_inner = f(&self.get());
        let cell = Cell::<U, CellMutable>::new(first_inner.get());

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
        let f_for_outer = f.clone();
        let first = Arc::new(AtomicBool::new(true));
        let outer_guard = self.subscribe(move |outer_value| {
            if first.swap(false, Ordering::SeqCst) {
                return;
            }

            let Some(c) = weak_outer.upgrade() else { return };

            let inner = f_for_outer(outer_value);

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

        // Complete when outer completes AND all inner cells complete
        // Track: outer_complete flag + count of active (non-complete) inner cells
        let outer_complete = Arc::new(AtomicBool::new(false));
        let active_inners = Arc::new(AtomicUsize::new(1)); // Start with 1 for first_inner

        // Track first inner completion
        let weak = cell.downgrade();
        let oc = outer_complete.clone();
        let ai = active_inners.clone();
        let first_inner_complete = first_inner.on_complete(move || {
            let remaining = ai.fetch_sub(1, Ordering::SeqCst) - 1;
            if remaining == 0 && oc.load(Ordering::SeqCst)
                && let Some(c) = weak.upgrade() {
                    c.complete();
                }
        });
        cell.own(first_inner_complete);

        // When outer creates new inners, track their completion too
        let weak_for_outer = cell.downgrade();
        let f2 = f;
        let oc2 = outer_complete.clone();
        let ai2 = active_inners.clone();
        let first2 = Arc::new(AtomicBool::new(true));
        let outer_complete_tracker = self.subscribe(move |outer_value| {
            if first2.swap(false, Ordering::SeqCst) {
                return;
            }

            let Some(c) = weak_for_outer.upgrade() else { return };

            // Increment active count before creating inner
            ai2.fetch_add(1, Ordering::SeqCst);

            let inner = f2(outer_value);

            // Track this inner's completion
            let weak_inner = weak_for_outer.clone();
            let oc_inner = oc2.clone();
            let ai_inner = ai2.clone();
            let inner_complete = inner.on_complete(move || {
                let remaining = ai_inner.fetch_sub(1, Ordering::SeqCst) - 1;
                if remaining == 0 && oc_inner.load(Ordering::SeqCst)
                    && let Some(c) = weak_inner.upgrade() {
                        c.complete();
                    }
            });
            c.own(inner_complete);
        });
        cell.own(outer_complete_tracker);

        // When outer completes, mark it and check if we can complete
        let weak = cell.downgrade();
        let outer_complete_guard = self.on_complete(move || {
            outer_complete.store(true, Ordering::SeqCst);
            if active_inners.load(Ordering::SeqCst) == 0
                && let Some(c) = weak.upgrade() {
                    c.complete();
                }
        });
        cell.own(outer_complete_guard);

        cell.lock()
    }
}

impl<T, W: Watchable<T>> MergeMapExt<T> for W {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MapExt;

    #[test]
    fn test_merge_map_merges() {
        let source = Cell::new(1u64);
        let merged = source.merge_map(|v| {
            // Must return CellImmutable, use map to create one
            Cell::new(*v).map(|x| x * 10)
        });

        assert_eq!(merged.get(), 10);
    }
}
