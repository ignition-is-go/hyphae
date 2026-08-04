use std::{
    marker::PhantomData,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use parking_lot::Mutex;
use uuid::Uuid;

use super::{CellValue, Watchable};
use crate::{
    cell::{Cell, CellMutable},
    pipeline::{Definite, MaterializeDefinite, Pipeline, PipelineInstall, PipelineSeed},
    signal::Signal,
    subscription::SubscriptionGuard,
};

// Lock-free completion state packed into a single u64:
// - Bits 0-61: generation (max 2^62-1)
// - Bit 62: inner_complete
// - Bit 63: outer_complete
const INNER_COMPLETE_BIT: u64 = 1 << 62;
const OUTER_COMPLETE_BIT: u64 = 1 << 63;
const GEN_MASK: u64 = (1 << 62) - 1;

pub struct SwitchMapPipeline<S, T, U, F, I> {
    source: S,
    f: Arc<F>,
    _types: PhantomData<fn(T) -> (U, I)>,
}

impl<S, T, U, F, I> PipelineInstall<U> for SwitchMapPipeline<S, T, U, F, I>
where
    S: PipelineInstall<T> + PipelineSeed<T>,
    T: CellValue,
    U: CellValue,
    F: Fn(&T) -> I + Send + Sync + 'static,
    I: PipelineInstall<U> + PipelineSeed<U>,
{
    fn install(&self, callback: Arc<dyn Fn(&Signal<U>) + Send + Sync>) -> SubscriptionGuard {
        let first_inner = (self.f)(&self.source.seed());
        let cell = Cell::<U, CellMutable>::new(first_inner.seed());

        // Stable key for the inner subscription guard so switch_map replaces (not accumulates)
        let inner_guard_key = Uuid::new_v4();

        // Packed state: generation (bits 0-61), inner_complete (bit 62), outer_complete (bit 63)
        // All completion logic uses CAS loops on this single atomic for lock-free operation
        let state = Arc::new(AtomicU64::new(0)); // gen 0, both incomplete

        // Serializes a switch (generation bump, in the outer handler) against a
        // still-live old inner's guard-check-then-emit. Under the scheduler's
        // wave-parallel drain the selector and an old inner are distinct
        // same-height cells that can run concurrently; without this an old inner
        // can read the current generation, pass its staleness guard, then have
        // its now-stale value win the output's last-write-wins coalescing slot
        // over the just-switched-in inner's value (a lost switch, confirmed by
        // repro). Holding this lock across {gen-check + emit} in an inner and
        // across the {gen-bump} in the selector makes the two mutually exclusive:
        // an old inner either emits fully before the switch (its value precedes
        // the new inner's seed, so the new one still wins the slot) or observes
        // the bumped generation and is rejected. The new inner is subscribed
        // *after* the lock is released, so its synchronous seed can't re-enter
        // (and deadlock on) this same lock.
        let switch_lock = Arc::new(Mutex::new(()));

        // Subscribe to first inner (generation 0)
        let weak = cell.downgrade();
        let state_for_first = state.clone();
        let switch_lock_first = switch_lock.clone();
        let first_guard = first_inner.install(Arc::new(move |signal| {
            let _switch = switch_lock_first.lock();
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
        }));
        cell.own_keyed(inner_guard_key, first_guard);

        // Single subscription to outer handles both value switching and completion tracking
        let weak = cell.downgrade();
        let f = self.f.clone();
        let state_for_outer = state.clone();
        let switch_lock_outer = switch_lock;
        let first = Arc::new(AtomicBool::new(true));
        let outer_guard = self.source.install(Arc::new(move |signal| {
            match signal {
                Signal::Value(outer_value) => {
                    if first.swap(false, Ordering::SeqCst) {
                        return;
                    }

                    // Per-outer-fire re-knit: rebuild the inner cell + re-subscribe
                    // + drop the prior inner guard. This span isolates switch_map's
                    // un-fusable teardown/rebuild cost from the anonymous
                    // `hyphae.fanout` aggregate. Compiles to nothing without `profiling`.
                    #[cfg(feature = "profiling")]
                    let _reknit_span = ::tracing::trace_span!("hyphae.switch_map").entered();

                    let Some(c) = weak.upgrade() else { return };

                    // Increment generation, clear inner_complete, preserve
                    // outer_complete — under `switch_lock` so it's atomic against
                    // an old inner's guard-check-then-emit. Released before the
                    // new inner is subscribed below, so the synchronous seed can't
                    // re-enter this lock.
                    let my_gen = {
                        let _switch = switch_lock_outer.lock();
                        loop {
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
                        }
                    };

                    let inner = f(outer_value.as_ref());

                    // Subscribe to new inner for values and completion
                    let weak_inner = weak.clone();
                    let state_for_inner = state_for_outer.clone();
                    let switch_lock_inner = switch_lock_outer.clone();
                    let value_guard = inner.install(Arc::new(move |signal| {
                        let _switch = switch_lock_inner.lock();
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
                    }));
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
        }));
        cell.own(outer_guard);

        cell.subscribe(move |signal| callback(signal))
    }
}

impl<S, T, U, F, I> PipelineSeed<U> for SwitchMapPipeline<S, T, U, F, I>
where
    S: PipelineSeed<T>,
    T: CellValue,
    U: CellValue,
    F: Fn(&T) -> I + Send + Sync + 'static,
    I: PipelineInstall<U> + PipelineSeed<U>,
{
    fn seed(&self) -> U {
        (self.f)(&self.source.seed()).seed()
    }
}
impl<S, T, U, F, I> Pipeline<U, Definite> for SwitchMapPipeline<S, T, U, F, I>
where
    S: Pipeline<T, Definite> + PipelineSeed<T>,
    T: CellValue,
    U: CellValue,
    F: Fn(&T) -> I + Send + Sync + 'static,
    I: Pipeline<U, Definite> + PipelineSeed<U>,
{
}
impl<S, T, U, F, I> MaterializeDefinite<U> for SwitchMapPipeline<S, T, U, F, I>
where
    S: Pipeline<T, Definite> + PipelineSeed<T>,
    T: CellValue,
    U: CellValue,
    F: Fn(&T) -> I + Send + Sync + 'static,
    I: Pipeline<U, Definite> + PipelineSeed<U>,
{
}

pub trait SwitchMapExt<T: CellValue>: Pipeline<T, Definite> + PipelineSeed<T> {
    fn switch_map<U, F, I>(self, f: F) -> SwitchMapPipeline<Self, T, U, F, I>
    where
        U: CellValue,
        F: Fn(&T) -> I + Send + Sync + 'static,
        I: Pipeline<U, Definite> + PipelineSeed<U>,
    {
        SwitchMapPipeline {
            source: self,
            f: Arc::new(f),
            _types: PhantomData,
        }
    }
}
impl<T: CellValue, P> SwitchMapExt<T> for P where P: Pipeline<T, Definite> + PipelineSeed<T> {}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use super::*;
    use crate::{Gettable, MapExt, MaterializeDefinite, Mutable};

    #[test]
    fn switch_map_does_not_build_an_inner_until_materialized() {
        let source = Cell::new(1u64);
        let calls = Arc::new(AtomicUsize::new(0));
        let inner_calls = calls.clone();
        let pipeline = source.switch_map(move |value| {
            inner_calls.fetch_add(1, Ordering::SeqCst);
            Cell::new(*value)
        });
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let _switched = pipeline.materialize();
        assert!(calls.load(Ordering::SeqCst) >= 2);
    }

    #[test]
    fn test_switch_map_switches() {
        let source = Cell::new(1u64);
        let switched = source
            .clone()
            .switch_map(|v| {
                let v = *v;
                Cell::new(v * 10).map(move |x| x + v)
            })
            .materialize();

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
        let switched = source
            .clone()
            .switch_map(move |v| {
                let v = *v;
                let count_inner = count.clone();
                // Simulate: query_map().items() — an intermediate cell
                let intermediate = Cell::new(v * 10);
                // Simulate: .map() on items
                intermediate
                    .map(move |x| {
                        count_inner.fetch_add(1, Ordering::SeqCst);
                        *x + v
                    })
                    .materialize()
            })
            .materialize();

        assert_eq!(switched.get(), 0); // 0 * 10 + 0
        let calls_after_init = map_call_count.load(Ordering::SeqCst);
        // Under the fused-pipeline model, materialize() runs the inner closure
        // twice per switch (once for self.get() to compute the initial cell
        // value, and once when install() subscribes synchronously).
        assert!(calls_after_init >= 1);

        // Switch — old inner map closure should stop being called
        source.set(1);
        assert_eq!(switched.get(), 11); // 1 * 10 + 1
        let calls_after_switch = map_call_count.load(Ordering::SeqCst);
        let calls_per_switch = calls_after_switch - calls_after_init;
        assert!(calls_per_switch >= 1);

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
            calls_after_init + 20 * calls_per_switch,
            "map called {} times after 20 switches, expected {} (old inner maps leaking if higher)",
            calls_after_20,
            calls_after_init + 20 * calls_per_switch
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
        let switched = source
            .clone()
            .switch_map(move |v| {
                let intermediate = Cell::new(*v * 10);
                collector.lock().unwrap().push(intermediate.downgrade());
                intermediate.lock()
            })
            .materialize();

        assert_eq!(switched.get(), 0);

        // Switch 20 times
        for i in 1..=20u64 {
            source.set(i);
        }
        assert_eq!(switched.get(), 200);

        let weaks = weak_collector.lock().unwrap();
        // Definite pipeline materialization evaluates the seed recipe once,
        // then installs an independent current inner. Both are expected; the
        // seed-only inner must already be dead.
        assert_eq!(weaks.len(), 22); // seed + installed initial + 20 switches

        // Only the last inner cell should be alive (the current one)
        let alive_count = weaks.iter().filter(|w| w.upgrade().is_some()).count();
        assert!(
            alive_count <= 1,
            "expected at most 1 live inner cell, found {} — old cells not being dropped",
            alive_count
        );
    }
}
