//! Exact-value correctness under sustained multi-thread contention.
//!
//! `scheduler_completeness` proves no-drop (event cells) and liveness. This
//! adds the property those don't: with many *independent* reactive graphs
//! driven concurrently through the one process-wide scheduler, every graph
//! settles on exactly the value its own inputs dictate — no cross-graph drop,
//! reorder, or torn combine leaks between them, and no update is lost.
//!
//! Independent graphs (each thread owns its own sources) means the correct
//! settled value is exactly computable per thread even though the settling all
//! shares one drain. Assertions run after every thread has joined, so the
//! scheduler is quiescent (the last batch to close at depth 0 drains to
//! completion) and every graph is fully settled.
#![cfg(feature = "scheduler")]

use std::{sync::Arc, thread};

use parking_lot::Mutex;

use hyphae::{
    Cell, CellMutable, JoinExt, MapExt, Materialize, Mutable, Signal, Watchable, batch,
    scheduler::no_coalesce,
};

const THREADS: usize = 8;
const ITERS: i64 = 4_000;
static SCHEDULER_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn scheduler_test_serial() -> std::sync::MutexGuard<'static, ()> {
    // Force the wave-parallel drain path at test width: production defaults
    // the group threshold high (waves stay sequential at rest), so parallelism
    // tests must lower it to actually exercise concurrent same-height groups.
    hyphae::scheduler::set_wave_threshold_for_test(4);
    SCHEDULER_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[test]
fn independent_diamonds_and_accumulators_settle_correctly_under_contention() {
    let _serial = scheduler_test_serial();
    // Per-thread observed state: (last diamond value, event-sum accumulator).
    let last: Vec<Arc<Mutex<i64>>> = (0..THREADS)
        .map(|_| Arc::new(Mutex::new(i64::MIN)))
        .collect();
    let acc: Vec<Arc<Mutex<i64>>> = (0..THREADS).map(|_| Arc::new(Mutex::new(0))).collect();

    thread::scope(|s| {
        for (last_value, accumulator) in last.iter().zip(&acc) {
            let last = Arc::clone(last_value);
            let acc = Arc::clone(accumulator);
            s.spawn(move || {
                // A diamond: k = (src+1) + (src*10) — a behavior graph whose
                // settled value is a pure function of the latest `src`.
                let src = Cell::new(0i64);
                let a = src
                    .clone()
                    .map(|value| value.saturating_add(1))
                    .materialize();
                let b = src
                    .clone()
                    .map(|value| value.saturating_mul(10))
                    .materialize();
                let k = a
                    .join(b)
                    .map(|(left, right)| left.saturating_add(*right))
                    .materialize();
                let sink = last.clone();
                let g1 = k.subscribe(move |sig| {
                    if let Signal::Value(v) = sig {
                        *sink.lock() = **v;
                    }
                });

                // An independent event-semantic accumulator on its own
                // no_coalesce source: every set must survive (no LWW drop), so
                // the sum is exact.
                let ev = no_coalesce(|| Cell::<i64, CellMutable>::new(0));
                let esink = acc.clone();
                let g2 = ev.clone().lock().subscribe(move |sig| {
                    if let Signal::Value(v) = sig {
                        let mut sum = esink.lock();
                        *sum = sum.saturating_add(**v);
                    }
                });
                *acc.lock() = 0; // discard subscribe-time replay

                for i in 1..=ITERS {
                    // Diamond and accumulator driven together in one batch.
                    batch(|| {
                        src.set(i);
                        ev.set(i);
                    });
                }
                std::mem::forget(g1);
                std::mem::forget(g2);
            });
        }
    });

    // Every thread has joined → scheduler quiescent → everything settled.
    let final_src = ITERS;
    let expected_diamond = final_src
        .saturating_add(1)
        .saturating_add(final_src.saturating_mul(10));
    let expected_sum: i64 = (1..=ITERS).sum();
    for (thread_index, (last_value, accumulator)) in last.iter().zip(&acc).enumerate() {
        assert_eq!(
            *last_value.lock(),
            expected_diamond,
            "thread {thread_index}: diamond settled on the wrong (torn/stale) value"
        );
        assert_eq!(
            *accumulator.lock(),
            expected_sum,
            "thread {thread_index}: event accumulator lost or double-counted a set"
        );
    }
}
