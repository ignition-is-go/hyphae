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

use std::{
    sync::{Arc, Mutex},
    thread,
};

use hyphae::{
    Cell, CellMutable, JoinExt, MapExt, MaterializeDefinite, Mutable, Signal, Watchable, batch,
    scheduler::no_coalesce,
};

fn scheduler_test_serial() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[test]
fn independent_diamonds_and_accumulators_settle_correctly_under_contention() {
    let _serial = scheduler_test_serial();
    const THREADS: i64 = 8;
    const ITERS: i64 = 4_000;

    // Per-thread observed state: (last diamond value, event-sum accumulator).
    let last: Vec<Arc<Mutex<i64>>> = (0..THREADS)
        .map(|_| Arc::new(Mutex::new(i64::MIN)))
        .collect();
    let acc: Vec<Arc<Mutex<i64>>> = (0..THREADS).map(|_| Arc::new(Mutex::new(0))).collect();

    thread::scope(|s| {
        for t in 0..THREADS {
            let last = last[t as usize].clone();
            let acc = acc[t as usize].clone();
            s.spawn(move || {
                // A diamond: k = (src+1) + (src*10) — a behavior graph whose
                // settled value is a pure function of the latest `src`.
                let src = Cell::new(0i64);
                let a = src.clone().map(|x| x + 1).materialize();
                let b = src.clone().map(|x| x * 10).materialize();
                let k = a.join(&b).map(|(x, y)| *x + *y).materialize();
                let sink = last.clone();
                let g1 = k.subscribe(move |sig| {
                    if let Signal::Value(v) = sig {
                        *sink.lock().unwrap() = **v;
                    }
                });

                // An independent event-semantic accumulator on its own
                // no_coalesce source: every set must survive (no LWW drop), so
                // the sum is exact.
                let ev = no_coalesce(|| Cell::<i64, CellMutable>::new(0));
                let esink = acc.clone();
                let g2 = ev.clone().lock().subscribe(move |sig| {
                    if let Signal::Value(v) = sig {
                        *esink.lock().unwrap() += **v;
                    }
                });
                *acc.lock().unwrap() = 0; // discard subscribe-time replay

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
    let expected_diamond = (final_src + 1) + (final_src * 10);
    let expected_sum: i64 = (1..=ITERS).sum();
    for t in 0..THREADS as usize {
        assert_eq!(
            *last[t].lock().unwrap(),
            expected_diamond,
            "thread {t}: diamond settled on the wrong (torn/stale) value"
        );
        assert_eq!(
            *acc[t].lock().unwrap(),
            expected_sum,
            "thread {t}: event accumulator lost or double-counted a set"
        );
    }
}
