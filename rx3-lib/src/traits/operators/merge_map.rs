use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use crate::cell::{Cell, CellImmutable, CellMutable};
use crate::signal::Signal;
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

        // Complete when outer completes AND all inner cells complete
        // Track: outer_complete flag + count of active (non-complete) inner cells
        let outer_complete = Arc::new(AtomicBool::new(false));
        let active_inners = Arc::new(AtomicUsize::new(1)); // Start with 1 for first_inner

        // Subscribe to first inner
        let weak = cell.downgrade();
        let oc = outer_complete.clone();
        let ai = active_inners.clone();
        let first_inner_guard = first_inner.subscribe(move |signal| {
            if let Some(c) = weak.upgrade() {
                match signal {
                    Signal::Value(_) => c.notify(signal.clone()),
                    Signal::Complete => {
                        let remaining = ai.fetch_sub(1, Ordering::SeqCst) - 1;
                        if remaining == 0 && oc.load(Ordering::SeqCst) {
                            c.notify(Signal::Complete);
                        }
                    }
                    Signal::Error(e) => c.notify(Signal::Error(e.clone())),
                }
            }
        });
        cell.own(first_inner_guard);

        // When outer changes, subscribe to new inner (without unsubscribing from previous)
        // Note: merge_map accumulates subscriptions by design - each inner cell stays subscribed
        let weak_outer = cell.downgrade();
        let f = Arc::new(f);
        let first = Arc::new(AtomicBool::new(true));
        let oc2 = outer_complete.clone();
        let ai2 = active_inners.clone();
        let outer_guard = self.subscribe(move |signal| {
            match signal {
                Signal::Value(outer_value) => {
                    if first.swap(false, Ordering::SeqCst) {
                        return;
                    }

                    let Some(c) = weak_outer.upgrade() else { return };

                    // Increment active count before creating inner
                    ai2.fetch_add(1, Ordering::SeqCst);

                    let inner = f(outer_value.as_ref());

                    // Subscribe to new inner - these subscriptions accumulate
                    let weak_inner = weak_outer.clone();
                    let oc_inner = oc2.clone();
                    let ai_inner = ai2.clone();
                    let inner_guard = inner.subscribe(move |signal| {
                        if let Some(c) = weak_inner.upgrade() {
                            match signal {
                                Signal::Value(_) => c.notify(signal.clone()),
                                Signal::Complete => {
                                    let remaining = ai_inner.fetch_sub(1, Ordering::SeqCst) - 1;
                                    if remaining == 0 && oc_inner.load(Ordering::SeqCst) {
                                        c.notify(Signal::Complete);
                                    }
                                }
                                Signal::Error(e) => c.notify(Signal::Error(e.clone())),
                            }
                        }
                    });
                    c.own(inner_guard);
                }
                Signal::Complete => {
                    outer_complete.store(true, Ordering::SeqCst);
                    if active_inners.load(Ordering::SeqCst) == 0 {
                        if let Some(c) = weak_outer.upgrade() {
                            c.notify(Signal::Complete);
                        }
                    }
                }
                Signal::Error(e) => {
                    if let Some(c) = weak_outer.upgrade() {
                        c.notify(Signal::Error(e.clone()));
                    }
                }
            }
        });
        cell.own(outer_guard);

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
