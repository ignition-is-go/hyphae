use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use crate::cell::{Cell, CellImmutable, CellMutable};
use super::{Gettable, Watchable};

pub trait SwitchMapExt<T>: Watchable<T> {
    fn switch_map<U, F>(&self, f: F) -> Cell<U, CellImmutable>
    where
        T: Clone + Send + Sync + 'static,
        U: Clone + Send + Sync + 'static,
        F: Fn(&T) -> Cell<U, CellImmutable> + Send + Sync + 'static,
        Self: Clone + Send + Sync + 'static,
    {
        let first_inner = f(&self.get());
        let cell = Cell::<U, CellMutable>::new(first_inner.get());

        // Generation counter - only the current generation emits
        let generation = Arc::new(AtomicU64::new(0));

        // Subscribe to first inner (generation 0)
        let weak = cell.downgrade();
        let gen_check = generation.clone();
        let first_guard = first_inner.subscribe(move |value| {
            if gen_check.load(Ordering::SeqCst) == 0 {
                if let Some(c) = weak.upgrade() {
                    c.notify(value.clone());
                }
            }
        });
        cell.own(first_guard);

        // When outer changes, switch to new inner
        let weak_outer = cell.downgrade();
        let f = Arc::new(f);
        let f_for_outer = f.clone();
        let first = Arc::new(AtomicBool::new(true));
        let gen_outer = generation.clone();
        let outer_guard = self.subscribe(move |outer_value| {
            if first.swap(false, Ordering::SeqCst) {
                return;
            }

            let Some(c) = weak_outer.upgrade() else { return };

            // Increment generation - old subscriptions become no-ops
            let my_gen = gen_outer.fetch_add(1, Ordering::SeqCst) + 1;

            let inner = f_for_outer(outer_value);

            // Subscribe to new inner with generation check
            let weak_inner = weak_outer.clone();
            let gen_check = gen_outer.clone();
            let guard = inner.subscribe(move |value| {
                if gen_check.load(Ordering::SeqCst) == my_gen {
                    if let Some(c) = weak_inner.upgrade() {
                        c.notify(value.clone());
                    }
                }
            });
            c.own(guard);
        });
        cell.own(outer_guard);

        // Complete when outer completes AND current inner completes
        let outer_complete = Arc::new(AtomicBool::new(false));
        let inner_complete = Arc::new(AtomicBool::new(false));
        let current_gen = Arc::new(AtomicU64::new(0));

        // Track first inner completion
        let weak = cell.downgrade();
        let oc = outer_complete.clone();
        let ic = inner_complete.clone();
        let cg = current_gen.clone();
        let first_inner_complete = first_inner.on_complete(move || {
            // Only count if still generation 0
            if cg.load(Ordering::SeqCst) == 0 {
                ic.store(true, Ordering::SeqCst);
                if oc.load(Ordering::SeqCst) {
                    if let Some(c) = weak.upgrade() {
                        c.complete();
                    }
                }
            }
        });
        cell.own(first_inner_complete);

        // Track when we switch to new inners
        let weak_for_switch = cell.downgrade();
        let f2 = f;
        let oc2 = outer_complete.clone();
        let ic2 = inner_complete.clone();
        let cg2 = current_gen.clone();
        let first2 = Arc::new(AtomicBool::new(true));
        let switch_tracker = self.subscribe(move |outer_value| {
            if first2.swap(false, Ordering::SeqCst) {
                return;
            }

            let Some(c) = weak_for_switch.upgrade() else { return };

            // Update current generation and reset inner_complete
            let my_gen = cg2.fetch_add(1, Ordering::SeqCst) + 1;
            ic2.store(false, Ordering::SeqCst);

            let inner = f2(outer_value);

            // Track this inner's completion
            let weak_inner = weak_for_switch.clone();
            let oc_inner = oc2.clone();
            let ic_inner = ic2.clone();
            let cg_inner = cg2.clone();
            let complete_guard = inner.on_complete(move || {
                // Only count if still current generation
                if cg_inner.load(Ordering::SeqCst) == my_gen {
                    ic_inner.store(true, Ordering::SeqCst);
                    if oc_inner.load(Ordering::SeqCst) {
                        if let Some(c) = weak_inner.upgrade() {
                            c.complete();
                        }
                    }
                }
            });
            c.own(complete_guard);
        });
        cell.own(switch_tracker);

        // When outer completes, mark it and check if we can complete
        let weak = cell.downgrade();
        let outer_complete_guard = self.on_complete(move || {
            outer_complete.store(true, Ordering::SeqCst);
            if inner_complete.load(Ordering::SeqCst) {
                if let Some(c) = weak.upgrade() {
                    c.complete();
                }
            }
        });
        cell.own(outer_complete_guard);

        cell.lock()
    }
}

impl<T, W: Watchable<T>> SwitchMapExt<T> for W {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MapExt;

    #[test]
    fn test_switch_map_switches() {
        let source = Cell::new(1u64);
        let switched = source.switch_map(|v| {
            let v = *v;
            Cell::new(v * 10).map(move |x| x + v)
        });

        // Initial: 1 * 10 + 1 = 11
        assert_eq!(switched.get(), 11);
    }
}
