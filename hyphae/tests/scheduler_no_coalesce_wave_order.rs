//! Does a `no_coalesce` (event) cell fired many times inside ONE batch keep
//! its events in arrival order — and deliver them one-at-a-time — when the
//! resulting wave is wide enough to dispatch in parallel?
//!
//! The scheduler keys deferred ops by the *notified cell's* id and keeps every
//! `no_coalesce` op as a distinct `(height, id, seq)` entry (last-write-wins is
//! suppressed for event cells). So N sets of one event source in a single batch
//! become N ops that all share `(height=0, id)` and land in the SAME wave. Once
//! N >= PARALLEL_WAVE_THRESHOLD (8), `run_wave` dispatches that wave across
//! rayon — running N fanouts of the *same* cell to the *same* subscribers
//! concurrently. `run_wave`'s independence invariant assumes each op writes a
//! distinct cell; N same-cell event ops violate it. This test pins down whether
//! that reorders or overlaps event delivery.
#![cfg(feature = "scheduler")]

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use hyphae::{Cell, CellMutable, Mutable, Signal, Watchable, batch, scheduler::no_coalesce};

const N: i64 = 32; // well over the parallel-wave threshold (8)
const ITERS: usize = 2_000;

#[test]
fn no_coalesce_burst_in_one_batch_keeps_events_ordered_and_serial() {
    // Force the parallel drain path: production defaults the wave threshold high.
    hyphae::scheduler::set_wave_threshold_for_test(4);
    let src = no_coalesce(|| Cell::<i64, CellMutable>::new(0));

    let received: Arc<Mutex<Vec<i64>>> = Arc::new(Mutex::new(Vec::new()));
    let in_flight = Arc::new(AtomicUsize::new(0));
    let max_in_flight = Arc::new(AtomicUsize::new(0));

    let r = received.clone();
    let inf = in_flight.clone();
    let maxf = max_in_flight.clone();
    let guard = src.clone().lock().subscribe(move |sig| {
        if let Signal::Value(v) = sig {
            // Direct detector: if the same subscriber runs concurrently with
            // itself, in_flight climbs above 1.
            let cur = inf.fetch_add(1, Ordering::SeqCst) + 1;
            maxf.fetch_max(cur, Ordering::SeqCst);
            // Widen the window so a genuine overlap is actually observed.
            for _ in 0..64 {
                std::hint::spin_loop();
            }
            r.lock().unwrap().push(**v);
            inf.fetch_sub(1, Ordering::SeqCst);
        }
    });

    let expected: Vec<i64> = (1..=N).collect();
    for it in 0..ITERS {
        received.lock().unwrap().clear();
        batch(|| {
            for i in 1..=N {
                src.set(i);
            }
        });
        let got = received.lock().unwrap().clone();
        assert_eq!(
            got.len(),
            N as usize,
            "iter {it}: a no_coalesce event was dropped"
        );
        assert_eq!(
            got, expected,
            "iter {it}: no_coalesce events delivered OUT OF ORDER under a parallel wave"
        );
    }

    std::mem::forget(guard);
    let peak = max_in_flight.load(Ordering::SeqCst);
    assert_eq!(
        peak, 1,
        "the same event subscriber ran concurrently with itself (peak in-flight = {peak})"
    );
}
