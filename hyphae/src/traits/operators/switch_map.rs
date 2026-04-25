use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

use uuid::Uuid;

use super::{CellValue, Gettable, Watchable};
use crate::{
    cell::{Cell, CellImmutable, CellMutable},
    signal::Signal,
};

// Lock-free completion state packed into a single u64:
// - Bits 0-61: generation (max 2^62-1)
// - Bit 62: inner_complete
// - Bit 63: outer_complete
const INNER_COMPLETE_BIT: u64 = 1 << 62;
const OUTER_COMPLETE_BIT: u64 = 1 << 63;
const GEN_MASK: u64 = (1 << 62) - 1;

pub trait SwitchMapExt<T>: Watchable<T> {
    #[track_caller]
    fn switch_map<U, F>(&self, f: F) -> Cell<U, CellImmutable>
    where
        T: CellValue,
        U: CellValue,
        F: Fn(&T) -> Cell<U, CellImmutable> + Send + Sync + 'static,
        Self: Clone + Send + Sync + 'static,
    {
        let first_inner = f(&self.get());
        let cell = Cell::<U, CellMutable>::new(first_inner.get());
        let cell = if let Some(name) = self.name() {
            cell.with_name(format!("{}::switch_map", name))
        } else {
            cell
        };

        // Stable key for the inner subscription guard so switch_map replaces (not accumulates)
        let inner_guard_key = Uuid::new_v4();

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
                    Signal::Value(_) => c.notify(signal.clone()),
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
        cell.own_keyed(inner_guard_key, first_guard);

        // Single subscription to outer handles both value switching and completion tracking
        let weak = cell.downgrade();
        let f = Arc::new(f);
        let state_for_outer = state.clone();
        let first = Arc::new(AtomicBool::new(true));
        let outer_guard = self.subscribe(move |signal| {
            match signal {
                Signal::Value(outer_value) => {
                    if first.swap(false, Ordering::SeqCst) {
                        return;
                    }

                    let Some(c) = weak.upgrade() else { return };

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
                                Signal::Value(_) => c.notify(signal.clone()),
                                Signal::Complete => loop {
                                    let old = state_for_inner.load(Ordering::SeqCst);
                                    if old & GEN_MASK != my_gen {
                                        return;
                                    }
                                    if old & INNER_COMPLETE_BIT != 0 {
                                        return;
                                    }
                                    let new = old | INNER_COMPLETE_BIT;
                                    if state_for_inner
                                        .compare_exchange(
                                            old,
                                            new,
                                            Ordering::SeqCst,
                                            Ordering::SeqCst,
                                        )
                                        .is_ok()
                                    {
                                        if new & OUTER_COMPLETE_BIT != 0 {
                                            c.notify(Signal::Complete);
                                        }
                                        return;
                                    }
                                },
                                Signal::Error(e) => c.notify(Signal::Error(e.clone())),
                            }
                        }
                    });
                    c.own_keyed(inner_guard_key, value_guard);
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
    use crate::{MapExt, Mutable, pipeline::Pipeline};

    #[test]
    fn test_switch_map_switches() {
        let source = Cell::new(1u64);
        let switched = source.switch_map(|v| {
            let v = *v;
            Cell::new(v * 10).map(move |x| x + v).materialize()
        });

        // Initial: 1 * 10 + 1 = 11
        assert_eq!(switched.get(), 11);
    }

    #[test]
    fn test_switch_map_inner_chain_with_map_drops() {
        // Matches the CuePaused report pattern: switch_map creates a new
        // inner cell chain (simulating query_map().items().map()) on each
        // outer emission. Old inner closures must stop being called.
        use std::sync::atomic::{AtomicUsize, Ordering};

        let map_call_count = Arc::new(AtomicUsize::new(0));
        let source = Cell::new(0u64);

        let count = map_call_count.clone();
        let switched = source.switch_map(move |v| {
            let v = *v;
            let count_inner = count.clone();
            // Simulate: query_map().items() — an intermediate cell
            let intermediate = Cell::new(v * 10);
            // Simulate: .map() on items
            intermediate.map(move |x| {
                count_inner.fetch_add(1, Ordering::SeqCst);
                *x + v
            }).materialize()
        });

        assert_eq!(switched.get(), 0); // 0 * 10 + 0
        let calls_after_init = map_call_count.load(Ordering::SeqCst);
        // Under the fused-pipeline model, materialize() runs the inner closure
        // twice per switch (once for self.get() to compute the initial cell
        // value, and once when install() subscribes synchronously).
        let calls_per_switch = calls_after_init;
        assert!(calls_per_switch >= 1);

        // Switch — old inner map closure should stop being called
        source.set(1);
        assert_eq!(switched.get(), 11); // 1 * 10 + 1
        let calls_after_switch = map_call_count.load(Ordering::SeqCst);
        assert_eq!(calls_after_switch, 2 * calls_per_switch); // Only the new inner map called

        // Mutate source several times and verify calls grow linearly, not quadratically
        for i in 2..=20u64 {
            source.set(i);
        }
        let calls_after_20 = map_call_count.load(Ordering::SeqCst);
        // 21 switches total (initial + 20 source.set), each doing `calls_per_switch`
        // closure invocations. If old inner maps leak, we'd see growth like
        // ~1+2+3+...+20 instead of linear.
        assert_eq!(
            calls_after_20,
            21 * calls_per_switch,
            "map called {} times after 20 switches, expected {} (old inner maps leaking if higher)",
            calls_after_20,
            21 * calls_per_switch
        );
    }

    #[test]
    fn test_switch_map_old_intermediate_cells_dropped() {
        // Verify that intermediate cells created inside switch_map are actually
        // deallocated when the outer switches. Uses weak refs to detect liveness.
        let source = Cell::new(0u64);
        // We need shared mutable access to collect weak refs from inside the closure
        let weak_collector: Arc<std::sync::Mutex<Vec<crate::cell::WeakCell<u64, CellMutable>>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));

        let collector = weak_collector.clone();
        let switched = source.switch_map(move |v| {
            let intermediate = Cell::new(*v * 10);
            collector.lock().unwrap().push(intermediate.downgrade());
            intermediate.lock()
        });

        assert_eq!(switched.get(), 0);

        // Switch 20 times
        for i in 1..=20u64 {
            source.set(i);
        }
        assert_eq!(switched.get(), 200);

        let weaks = weak_collector.lock().unwrap();
        assert_eq!(weaks.len(), 21); // 1 initial + 20 switches

        // Only the last inner cell should be alive (the current one)
        let alive_count = weaks.iter().filter(|w| w.upgrade().is_some()).count();
        assert!(
            alive_count <= 1,
            "expected at most 1 live inner cell, found {} — old cells not being dropped",
            alive_count
        );
    }
}
