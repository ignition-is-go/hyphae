//! Regression coverage for the shared native timer reactor
//! (`platform::native::{spawn_interval, spawn_delayed}`).
//!
//! The reactor exists to make hyphae's `scheduler` feature's glitch-free
//! coalescing hold *process-wide*: that guarantee is thread-local (see
//! `crate::scheduler`), so any two timer-driven sources that used to spawn
//! their own OS thread could fan out into a shared downstream cell from
//! different threads with no coordination between them — silently dropping
//! updates at the convergence point. Routing every interval/delayed timer
//! through one reactor thread closes that hole. The first test below is the
//! actual regression gate for that bug; the rest cover that the reactor
//! didn't lose basic interval/delayed-timer correctness in the rewrite.

#![cfg(all(not(target_arch = "wasm32"), feature = "scheduler"))]

use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    thread::ThreadId,
    time::Duration,
};

use parking_lot::Mutex;

use hyphae::{Cell, DelayExt, Materialize, Mutable, Signal, Watchable, interval_source};

const DELAYED_TIMER_COUNT: usize = 200;
static SCHEDULER_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The timer reactor thread is a single process-wide singleton (that's the
/// point of this file). Serializing these tests isn't needed for
/// correctness — each test's assertions only look at its own timers — but it
/// keeps timing-margin assertions from getting flaky under a reactor that's
/// also busy servicing other tests' concurrently-registered timers.
fn scheduler_test_serial() -> std::sync::MutexGuard<'static, ()> {
    // Force the wave-parallel drain path at test width: production defaults
    // the group threshold high (waves stay sequential at rest), so parallelism
    // tests must lower it to actually exercise concurrent same-height groups.
    hyphae::scheduler::set_wave_threshold_for_test(4);
    SCHEDULER_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Two independently-created interval sources at different periods must fire
/// their subscribers from the SAME OS thread. Before the shared reactor, each
/// `spawn_interval` call spawned its own dedicated thread, so this would have
/// observed two distinct `ThreadId`s.
#[test]
fn independent_intervals_share_one_reactor_thread() {
    let _serial = scheduler_test_serial();
    let fast = interval_source(Duration::from_millis(5));
    let slow = interval_source(Duration::from_millis(9));

    let seen: Arc<Mutex<Vec<ThreadId>>> = Arc::new(Mutex::new(Vec::new()));

    let seen_fast = seen.clone();
    let guard_fast = fast.subscribe(move |signal| {
        if matches!(signal, Signal::Value(_)) {
            seen_fast.lock().push(std::thread::current().id());
        }
    });

    let seen_slow = seen.clone();
    let guard_slow = slow.subscribe(move |signal| {
        if matches!(signal, Signal::Value(_)) {
            seen_slow.lock().push(std::thread::current().id());
        }
    });

    std::thread::sleep(Duration::from_millis(120));
    drop(guard_fast);
    drop(guard_slow);

    let seen = seen.lock();
    assert!(
        seen.len() >= 4,
        "expected both intervals to have fired several times, got {} events",
        seen.len()
    );
    let distinct: std::collections::HashSet<_> = seen.iter().copied().collect();
    let distinct_len = distinct.len();
    drop(seen);
    assert_eq!(
        distinct_len, 1,
        "interval callbacks fired from {distinct_len} distinct threads, expected 1 (shared reactor): {distinct:?}"
    );
}

/// A dropped interval source's timer must actually stop being serviced (no
/// leaked forever-rescheduling entry in the shared reactor).
#[test]
fn dropped_interval_stops_firing() {
    let _serial = scheduler_test_serial();
    let count = Arc::new(AtomicUsize::new(0));
    let count_cb = count.clone();

    {
        let source = interval_source(Duration::from_millis(5));
        let _guard = source.subscribe(move |signal| {
            if matches!(signal, Signal::Value(_)) {
                count_cb.fetch_add(1, Ordering::SeqCst);
            }
        });
        std::thread::sleep(Duration::from_millis(40));
        // `source` and `_guard` drop here.
    }

    let after_drop = count.load(Ordering::SeqCst);
    assert!(after_drop > 0, "interval never fired before drop");

    std::thread::sleep(Duration::from_millis(60));
    let after_wait = count.load(Ordering::SeqCst);
    assert_eq!(
        after_wait, after_drop,
        "interval kept firing after its source was dropped"
    );
}

/// Many `.delay()` timers registered concurrently from multiple threads (the
/// debounce/throttle/timeout operator pattern under real, parallel load) all
/// still fire — proves the shared reactor's registration path is safe under
/// concurrent callers, not just sequential ones.
#[test]
fn burst_of_delayed_timers_all_fire() {
    let _serial = scheduler_test_serial();
    let fired = Arc::new(AtomicUsize::new(0));

    let handles: Vec<_> = (0..DELAYED_TIMER_COUNT)
        .map(|i| {
            let fired = fired.clone();
            std::thread::spawn(move || {
                let source = Cell::new(0u64);
                let delayed = source
                    .clone()
                    .delay(Duration::from_millis(5u64.saturating_add(
                        u64::try_from(i.rem_euclid(7)).unwrap_or(u64::MAX),
                    )))
                    .materialize();
                let fired = fired;
                let guard = delayed.subscribe(move |signal| {
                    if let Signal::Value(v) = signal
                        && **v == 42
                    {
                        fired.fetch_add(1, Ordering::SeqCst);
                    }
                });
                source.set(42);
                std::mem::forget(guard);
                std::mem::forget(source);
            })
        })
        .collect();

    for handle in handles {
        assert!(handle.join().is_ok());
    }

    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(fired.load(Ordering::SeqCst), DELAYED_TIMER_COUNT);
}
