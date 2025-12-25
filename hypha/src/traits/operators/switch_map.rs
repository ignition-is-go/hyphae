use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use tracing::trace;

use crate::cell::{Cell, CellImmutable, CellMutable};
use crate::signal::Signal;
use super::{Gettable, Watchable};

// Lock-free completion state packed into a single u64:
// - Bits 0-61: generation (max 2^62-1)
// - Bit 62: inner_complete
// - Bit 63: outer_complete
const INNER_COMPLETE_BIT: u64 = 1 << 62;
const OUTER_COMPLETE_BIT: u64 = 1 << 63;
const GEN_MASK: u64 = (1 << 62) - 1;

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
        let cell_id = cell.id();

        // Packed state: generation (bits 0-61), inner_complete (bit 62), outer_complete (bit 63)
        // All completion logic uses CAS loops on this single atomic for lock-free operation
        let state = Arc::new(AtomicU64::new(0)); // gen 0, both incomplete

        // Subscribe to first inner (generation 0)
        let weak = cell.downgrade();
        let state_for_first = state.clone();
        let first_guard = first_inner.subscribe(move |signal| {
            let current = state_for_first.load(Ordering::SeqCst);
            if current & GEN_MASK != 0 {
                return; // Generation changed, not current
            }
            if let Some(c) = weak.upgrade() {
                match signal {
                    Signal::Value(value, ctx) => {
                        // Propagate transaction context, updating path
                        let new_ctx = ctx.as_ref().and_then(|c| c.push(cell_id));
                        c.notify(Signal::Value(value.clone(), new_ctx));
                    }
                    Signal::Complete => {
                        // Set inner complete bit with CAS loop
                        loop {
                            let old = state_for_first.load(Ordering::SeqCst);
                            if old & GEN_MASK != 0 {
                                return; // Generation changed
                            }
                            if old & INNER_COMPLETE_BIT != 0 {
                                return; // Already marked complete
                            }
                            let new = old | INNER_COMPLETE_BIT;
                            if state_for_first
                                .compare_exchange(old, new, Ordering::SeqCst, Ordering::SeqCst)
                                .is_ok()
                            {
                                if new & OUTER_COMPLETE_BIT != 0 {
                                    c.notify(Signal::Complete);
                                }
                                return;
                            }
                        }
                    }
                    Signal::Error(e) => c.notify(Signal::Error(e.clone())),
                }
            }
        });
        cell.own(first_guard);

        // Single subscription to outer handles both value switching and completion tracking
        let weak = cell.downgrade();
        let f = Arc::new(f);
        let state_for_outer = state.clone();
        let first = Arc::new(AtomicBool::new(true));
        let outer_guard = self.subscribe(move |signal| {
            match signal {
                Signal::Value(outer_value, outer_ctx) => {
                    if first.swap(false, Ordering::SeqCst) {
                        return;
                    }

                    let Some(c) = weak.upgrade() else { return };

                    // Log transaction context for switch_map
                    if let Some(ctx) = &outer_ctx {
                        trace!(
                            cell = %cell_id,
                            txid = %ctx.txid,
                            "switch_map switching inner cell mid-transaction"
                        );
                    }

                    // Increment generation, clear inner_complete, preserve outer_complete
                    let my_gen = loop {
                        let old = state_for_outer.load(Ordering::SeqCst);
                        let outer_bit = old & OUTER_COMPLETE_BIT;
                        let old_gen = old & GEN_MASK;
                        let new_gen = old_gen + 1;
                        let new = new_gen | outer_bit; // new gen, outer preserved, inner cleared
                        if state_for_outer
                            .compare_exchange(old, new, Ordering::SeqCst, Ordering::SeqCst)
                            .is_ok()
                        {
                            break new_gen;
                        }
                    };

                    let inner = f(outer_value.as_ref());

                    // Subscribe to new inner for values and completion
                    let weak_inner = weak.clone();
                    let state_for_inner = state_for_outer.clone();
                    let value_guard = inner.subscribe(move |signal| {
                        let current = state_for_inner.load(Ordering::SeqCst);
                        if current & GEN_MASK != my_gen {
                            return; // Generation changed, not current
                        }
                        if let Some(c) = weak_inner.upgrade() {
                            match signal {
                                Signal::Value(value, ctx) => {
                                    // Propagate transaction context, updating path
                                    let new_ctx = ctx.as_ref().and_then(|c| c.push(cell_id));
                                    c.notify(Signal::Value(value.clone(), new_ctx));
                                }
                                Signal::Complete => {
                                    loop {
                                        let old = state_for_inner.load(Ordering::SeqCst);
                                        if old & GEN_MASK != my_gen {
                                            return;
                                        }
                                        if old & INNER_COMPLETE_BIT != 0 {
                                            return;
                                        }
                                        let new = old | INNER_COMPLETE_BIT;
                                        if state_for_inner
                                            .compare_exchange(old, new, Ordering::SeqCst, Ordering::SeqCst)
                                            .is_ok()
                                        {
                                            if new & OUTER_COMPLETE_BIT != 0 {
                                                c.notify(Signal::Complete);
                                            }
                                            return;
                                        }
                                    }
                                }
                                Signal::Error(e) => c.notify(Signal::Error(e.clone())),
                            }
                        }
                    });
                    c.own(value_guard);
                }
                Signal::Complete => {
                    // Set outer complete bit with CAS loop
                    loop {
                        let old = state_for_outer.load(Ordering::SeqCst);
                        if old & OUTER_COMPLETE_BIT != 0 {
                            return;
                        }
                        let new = old | OUTER_COMPLETE_BIT;
                        if state_for_outer
                            .compare_exchange(old, new, Ordering::SeqCst, Ordering::SeqCst)
                            .is_ok()
                        {
                            if new & INNER_COMPLETE_BIT != 0
                                && let Some(c) = weak.upgrade()
                            {
                                c.notify(Signal::Complete);
                            }
                            return;
                        }
                    }
                }
                Signal::Error(e) => {
                    if let Some(c) = weak.upgrade() {
                        c.notify(Signal::Error(e.clone()));
                    }
                }
            }
        });
        cell.own(outer_guard);

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
