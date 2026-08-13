use std::{
    marker::PhantomData,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use parking_lot::Mutex;
use uuid::Uuid;

use super::{CellValue, Watchable};
use crate::{
    cell::{Cell, CellMutable, WeakCell},
    pipeline::{Definite, Pipeline, PipelineInstall, PipelineSeed, prepare_install},
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

fn advance_generation(state: &AtomicU64) -> u64 {
    loop {
        let old = state.load(Ordering::SeqCst);
        let new_generation = (old & GEN_MASK).wrapping_add(1) & GEN_MASK;
        let new = new_generation | (old & OUTER_COMPLETE_BIT);
        if state
            .compare_exchange(old, new, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            return new_generation;
        }
    }
}

fn mark_inner_complete(state: &AtomicU64, generation: u64) -> bool {
    loop {
        let old = state.load(Ordering::SeqCst);
        if old & GEN_MASK != generation || old & INNER_COMPLETE_BIT != 0 {
            return false;
        }
        let new = old | INNER_COMPLETE_BIT;
        if state
            .compare_exchange(old, new, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            return new & OUTER_COMPLETE_BIT != 0;
        }
    }
}

fn mark_outer_complete(state: &AtomicU64) -> bool {
    loop {
        let old = state.load(Ordering::SeqCst);
        if old & OUTER_COMPLETE_BIT != 0 {
            return false;
        }
        let new = old | OUTER_COMPLETE_BIT;
        if state
            .compare_exchange(old, new, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            return new & INNER_COMPLETE_BIT != 0;
        }
    }
}

fn inner_callback<U: CellValue>(
    weak: WeakCell<U, CellMutable>,
    state: Arc<AtomicU64>,
    switch_lock: Arc<Mutex<()>>,
    generation: u64,
) -> Arc<dyn Fn(&Signal<U>) + Send + Sync> {
    Arc::new(move |signal| {
        let _switch = switch_lock.lock();
        if state.load(Ordering::SeqCst) & GEN_MASK != generation {
            return;
        }
        if let Some(cell) = weak.upgrade() {
            match signal {
                Signal::Value(_) => cell.notify(signal.clone()),
                Signal::Complete if mark_inner_complete(&state, generation) => {
                    cell.notify(Signal::Complete);
                }
                Signal::Complete => {}
                Signal::Error(error) => cell.notify(Signal::Error(error.clone())),
            }
        }
    })
}

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
        // Subscribe to the selector before choosing its initial value. This
        // closes the old seed/build-inner/subscribe window in which a topology
        // update could become the replay that we then blindly discarded.
        let outer_prepared = prepare_install(&self.source);
        let first_inner = (self.f)(outer_prepared.initial());
        let first_prepared = prepare_install(&first_inner);
        let cell = Cell::<U, CellMutable>::new(first_prepared.initial().clone());

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
        // Source callbacks may run concurrently. Serializing complete re-knits
        // ensures an older installation can never return after a newer one and
        // overwrite the newer generation's keyed guard.
        let reknit_lock = Arc::new(Mutex::new(()));

        // Subscribe to first inner (generation 0)
        let first_callback = inner_callback(
            cell.downgrade(),
            Arc::clone(&state),
            Arc::clone(&switch_lock),
            0,
        );
        let first_guard = first_prepared.activate(&first_callback);
        cell.own_keyed(inner_guard_key, first_guard);

        // Single subscription to outer handles both value switching and completion tracking
        let weak = cell.downgrade();
        let f = self.f.clone();
        let state_for_outer = state;
        let switch_lock_outer = switch_lock;
        let reknit_lock_outer = reknit_lock;
        let outer_callback: Arc<dyn Fn(&Signal<T>) + Send + Sync> = Arc::new(move |signal| {
            let _reknit = reknit_lock_outer.lock();
            match signal {
                Signal::Value(outer_value) => {
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
                        advance_generation(&state_for_outer)
                    };

                    let inner = f(outer_value.as_ref());
                    let prepared = prepare_install(&inner);

                    // Subscribe to new inner for values and completion
                    // Publish the prepared inner's freshest current value before
                    // activating delivery of anything that arrived afterward.
                    {
                        let _switch = switch_lock_outer.lock();
                        if state_for_outer.load(Ordering::SeqCst) & GEN_MASK == my_gen {
                            c.notify(Signal::value(prepared.initial().clone()));
                        }
                    }
                    let value_callback = inner_callback(
                        weak.clone(),
                        Arc::clone(&state_for_outer),
                        Arc::clone(&switch_lock_outer),
                        my_gen,
                    );
                    let value_guard = prepared.activate(&value_callback);
                    c.own_keyed(inner_guard_key, value_guard);
                }
                Signal::Complete => {
                    if mark_outer_complete(&state_for_outer)
                        && let Some(cell) = weak.upgrade()
                    {
                        cell.notify(Signal::Complete);
                    }
                }
                Signal::Error(e) => {
                    if let Some(c) = weak.upgrade() {
                        c.notify(Signal::Error(e.clone()));
                    }
                }
            }
        });
        let outer_guard = outer_prepared.activate(&outer_callback);
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

pub trait SwitchMapExt<T: CellValue>: Pipeline<T, Definite> + PipelineSeed<T> {
    fn switch_map<U, F, I>(self, f: F) -> impl crate::Materialize<U, Definite>
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
    use crate::{Gettable, MapExt, Materialize, Mutable};

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
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn test_switch_map_switches() {
        let source = Cell::new(1u64);
        let switched = source
            .switch_map(|v| {
                let v = *v;
                Cell::new(v * 10).map(move |x| x + v).materialize()
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
        assert_eq!(calls_after_init, 1);

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
                collector
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(intermediate.downgrade());
                intermediate.lock()
            })
            .materialize();

        assert_eq!(switched.get(), 0);

        // Switch 20 times
        for i in 1..=20u64 {
            source.set(i);
        }
        assert_eq!(switched.get(), 200);

        let (weak_count, alive_count) = {
            let weaks = weak_collector
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            (
                weaks.len(),
                weaks.iter().filter(|w| w.upgrade().is_some()).count(),
            )
        };
        assert_eq!(weak_count, 21); // installed initial + 20 switches

        // Only the last inner cell should be alive (the current one)
        assert!(
            alive_count <= 1,
            "expected at most 1 live inner cell, found {alive_count} — old cells not being dropped"
        );
    }
}
