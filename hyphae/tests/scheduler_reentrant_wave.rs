//! Coverage for the one nesting shape the completeness suite left untested
//! (flagged in the scheduler rewrite handoff): a `batch()` opened *reentrantly
//! from inside a rayon-dispatched parallel wave op* — as opposed to from an
//! independent thread, which `joiner_batches_never_hang` already covers.
//!
//! The hazard if the depth-tracking got the calling-context wrong: an op
//! running on a rayon worker mid-drain opens a nested `batch()`; that call
//! must join the existing active window (depth++, non-outermost, trust the
//! drainer) and never try to open/drain a second window from under the
//! drainer's feet — which would either deadlock on `TICK` or strand the
//! reentrant work.
#![cfg(feature = "scheduler")]

use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

use hyphae::{
    Cell, CellMutable, MapExt, MaterializeDefinite, Mutable, Signal, Watchable, batch,
    scheduler::no_coalesce,
};

fn scheduler_test_serial() -> std::sync::MutexGuard<'static, ()> {
    // Force the wave-parallel drain path at test width: production defaults
    // the group threshold high (waves stay sequential at rest), so parallelism
    // tests must lower it to actually exercise concurrent same-height groups.
    hyphae::scheduler::set_wave_threshold_for_test(4);
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[test]
fn reentrant_batch_from_inside_a_parallel_wave_never_hangs_and_drops_nothing() {
    let _serial = scheduler_test_serial();

    // N > PARALLEL_WAVE_THRESHOLD (8) so a single `s.set` produces a genuinely
    // rayon-dispatched wave: all N mapped cells are height 1 (each depends only
    // on `s`), so they settle in one parallel wave.
    const N: i64 = 16;
    const ITERATIONS: i64 = 500;

    let s = Cell::new(0i64);

    // `target` is no_coalesce so we can assert an exact survival count — every
    // reentrant set must reach it, none last-write-wins-dropped or stranded.
    let target = no_coalesce(|| Cell::<i64, CellMutable>::new(0));
    let observed = Arc::new(AtomicU64::new(0));
    let obs = observed.clone();
    let seen_first = Arc::new(AtomicBool::new(false));
    let g_target = target.clone().lock().subscribe(move |sig| {
        if matches!(sig, Signal::Value(_)) {
            if !seen_first.swap(true, Ordering::SeqCst) {
                return; // skip subscribe-time replay
            }
            obs.fetch_add(1, Ordering::SeqCst);
        }
    });

    // Each height-1 cell's subscriber opens a NESTED batch() — this callback
    // runs as part of that cell's fanout, which runs inside `run_wave`, i.e.
    // on a rayon worker thread when the wave is wide. That nested batch() is
    // the exact reentrant-from-parallel-dispatch shape under test.
    let mut guards = Vec::new();
    for k in 0..N {
        let cell = s.clone().map(move |x| x * (k + 1)).materialize();
        let t = target.clone();
        // Skip this subscriber's own subscribe-time replay so only the
        // ITERATIONS driven waves below are counted.
        let skip_first = Arc::new(AtomicBool::new(false));
        let g = cell.subscribe(move |sig| {
            if let Signal::Value(v) = sig {
                if !skip_first.swap(true, Ordering::SeqCst) {
                    return;
                }
                let val = **v;
                batch(|| t.set(val));
            }
        });
        // Keep both the mapped cell and its subscription alive for the run.
        guards.push((cell, g));
    }

    for i in 1..=ITERATIONS {
        batch(|| s.set(i));
    }

    // Liveness: reaching here at all means no reentrant batch() deadlocked or
    // hung. Non-drop: every iteration fires all N subscribers, each doing one
    // reentrant no_coalesce set, and the first-emit skip only elides the
    // subscribe-time replay (a single emission), so exactly N*ITERATIONS sets
    // must have been observed.
    let got = observed.load(Ordering::SeqCst);
    let expected = (N * ITERATIONS) as u64;
    assert_eq!(
        got, expected,
        "reentrant-from-wave sets dropped/stranded: observed {got}, expected {expected}"
    );

    drop(g_target);
    drop(guards);
}
