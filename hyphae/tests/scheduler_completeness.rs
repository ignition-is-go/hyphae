//! Completeness, liveness, and correctness under sustained concurrent
//! contention.
//!
//! `benches/scheduler_contention.rs` measures *throughput* under contention —
//! it never asserts that anything landed correctly. These tests check the
//! properties throughput numbers can't:
//!
//! - For a `no_coalesce` (event-semantic) cell, every enqueued op must be
//!   processed exactly once — none silently dropped/stranded, none
//!   double-counted. "No drops" is the right correctness criterion there
//!   because every distinct notify is meant to survive.
//! - For a coalescing (behavior-semantic) multi-input operator like `join`,
//!   "no drops" is the *wrong* criterion — dropping superseded intermediates
//!   is the entire point of coalescing. The right criterion is that the
//!   settled value is always correct: it reflects the most recently set
//!   inputs, never a torn mix of an old snapshot of one input and a new
//!   snapshot of another. That's a stronger, more useful check under genuine
//!   concurrent wave execution than counting arrivals.
//! - No `batch()` call — including a "joiner" that isn't the drainer for its
//!   active window — ever gets stuck waiting forever.
#![cfg(feature = "scheduler")]

use parking_lot::Mutex;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::Instant,
};

use hyphae::{
    Cell, CellMutable, MapExt, Materialize, Mutable, Signal, Watchable, batch, join_vec,
    scheduler::no_coalesce,
};

/// The scheduler's tick queue is a single process-wide structure (see
/// `hyphae::scheduler`'s cross-thread docs), so tests that exercise `batch()`
/// timing/contention would otherwise interfere with each other when `cargo
/// test` runs this file's `#[test]` fns as concurrent threads. Serialize them.
static SCHEDULER_TEST_LOCK: Mutex<()> = Mutex::new(());
const CONTENTION_THREADS: usize = 16;
const CONTENTION_OPS_PER_THREAD: u64 = 50_000;
const JOINER_THREADS: usize = 8;
const JOINER_OPS_PER_THREAD: usize = 5_000;
const WIDE_WAVE_SIZE: usize = 16;
const WIDE_WAVE_ITERATIONS: i64 = 2_000;

fn scheduler_test_serial() -> parking_lot::MutexGuard<'static, ()> {
    // Force the wave-parallel drain path at test width: production defaults
    // the group threshold high (waves stay sequential at rest), so parallelism
    // tests must lower it to actually exercise concurrent same-height groups.
    hyphae::scheduler::set_wave_threshold_for_test(4);
    SCHEDULER_TEST_LOCK.lock()
}

#[test]
fn no_op_is_dropped_or_stranded_under_sustained_contention() {
    let _serial = scheduler_test_serial();
    let expected = u64::try_from(CONTENTION_THREADS)
        .unwrap_or(u64::MAX)
        .saturating_mul(CONTENTION_OPS_PER_THREAD);

    // `no_coalesce`: every distinct notify must survive and be individually
    // observed. On a plain coalescing cell, "some updates get last-write-wins
    // dropped" is correct behavior by design, not a bug, so it couldn't tell
    // "working as intended" apart from "silently stranded."
    let sink = no_coalesce(|| Cell::<u64, CellMutable>::new(0));
    let observed = Arc::new(AtomicU64::new(0));
    let obs = observed.clone();
    let seen_first = Arc::new(AtomicBool::new(false));
    let guard = sink.clone().lock().subscribe(move |sig| {
        if matches!(sig, Signal::Value(_)) {
            if !seen_first.swap(true, Ordering::SeqCst) {
                return; // skip the initial subscribe-time replay
            }
            obs.fetch_add(1, Ordering::SeqCst);
        }
    });

    let start = Instant::now();
    thread::scope(|s| {
        for t in 0..CONTENTION_THREADS {
            let sink = sink.clone();
            s.spawn(move || {
                for i in 0..CONTENTION_OPS_PER_THREAD {
                    batch(|| sink.set(u64::try_from(t).unwrap_or(u64::MAX) << 32 | i));
                }
            });
        }
    });
    let elapsed = start.elapsed();

    let got = observed.load(Ordering::SeqCst);
    eprintln!(
        "observed={got} expected={expected} elapsed={elapsed:?} ({CONTENTION_THREADS} threads x {CONTENTION_OPS_PER_THREAD} ops)"
    );
    assert_eq!(
        got, expected,
        "some ops were dropped/stranded under concurrent contention (or double-counted)"
    );
    drop(guard);
}

#[test]
fn joiner_batches_never_hang_under_sustained_pressure() {
    let _serial = scheduler_test_serial();
    // A dedicated background thread hammers `batch()` continuously for the
    // whole test, so every other thread's `batch()` call is very likely to
    // land as a non-outermost "joiner" at some point (contributing depth,
    // trusting the drainer, never draining itself). If a joiner could ever
    // get stuck (e.g. a missed `notify_all` leaving it parked forever), this
    // reliably hangs instead of completing — `thread::scope`'s join below
    // never returns, so the test times out rather than failing cleanly, but
    // either way it doesn't silently pass.
    let stop = Arc::new(AtomicBool::new(false));
    let bg_stop = stop.clone();
    let bg_cell = no_coalesce(|| Cell::<u64, CellMutable>::new(0));
    let bg_handle = {
        let bg_cell = bg_cell.clone();
        thread::spawn(move || {
            let mut i = 0u64;
            while !bg_stop.load(Ordering::Relaxed) {
                batch(|| bg_cell.set(i));
                i = i.saturating_add(1);
            }
        })
    };

    let completed = Arc::new(AtomicU64::new(0));

    thread::scope(|s| {
        for _ in 0..JOINER_THREADS {
            let completed = completed.clone();
            let cell = bg_cell.clone();
            s.spawn(move || {
                for i in 0..JOINER_OPS_PER_THREAD {
                    batch(|| cell.set(u64::try_from(i).unwrap_or(u64::MAX)));
                    completed.fetch_add(1, Ordering::SeqCst);
                }
            });
        }
    });

    stop.store(true, Ordering::Relaxed);
    assert!(bg_handle.join().is_ok(), "background batch thread panicked");

    assert_eq!(
        completed.load(Ordering::SeqCst),
        u64::try_from(JOINER_THREADS.saturating_mul(JOINER_OPS_PER_THREAD)).unwrap_or(u64::MAX),
        "a joiner batch() call never returned (would have hung the thread::scope join above)"
    );
}

#[test]
fn join_vec_wide_wave_settles_correctly_under_repeated_concurrent_execution() {
    let _serial = scheduler_test_serial();
    // 16 cells > PARALLEL_WAVE_THRESHOLD (8 in the current scheduler), so
    // every source change below forces a genuinely parallel wave, not the
    // small-wave sequential fallback. Each cell's mapping is a pure,
    // deterministic function of the shared source, so the correct settled
    // value after each `batch()` is exactly computable — this is a strict
    // per-iteration equality check, not a "some fraction were torn"
    // probability estimate.
    let s = Cell::new(0i64);
    let cells: Vec<_> = (0..WIDE_WAVE_SIZE)
        .map(|k| {
            let factor = i64::try_from(k).unwrap_or(i64::MAX).saturating_add(1);
            s.clone().map(move |x| x * factor).materialize()
        })
        .collect();
    let combined = join_vec(cells).materialize();
    let last = Arc::new(Mutex::new(vec![0i64; WIDE_WAVE_SIZE]));
    let sink = last.clone();
    let guard = combined.subscribe(move |sig| {
        if let Signal::Value(v) = sig {
            *sink.lock() = (**v).clone();
        }
    });

    for i in 0..WIDE_WAVE_ITERATIONS {
        batch(|| s.set(i));
        let expected: Vec<i64> = (0..WIDE_WAVE_SIZE)
            .map(|k| i * i64::try_from(k).unwrap_or(i64::MAX).saturating_add(1))
            .collect();
        assert_eq!(
            *last.lock(),
            expected,
            "join_vec settled on a value inconsistent with the most recently set input at i={i}"
        );
    }
    drop(guard);
}
