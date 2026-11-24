//! A child domain whose propagation panics fails: it records the cause,
//! closes, and takes no more work, instead of carrying on with state that no
//! longer follows the writes it was given.

#![allow(clippy::panic, clippy::expect_used)]

use std::{
    sync::{
        Arc,
        atomic::{AtomicI64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

mod domain_support;

use domain_support::{DEADLINE, ROOT_TICK_SERIAL};
use hyphae::{Cell, Mutable, Signal, Watchable, scheduler::Domain};

fn wait_until(condition: impl Fn() -> bool) -> bool {
    let deadline = Instant::now().checked_add(DEADLINE);
    while !condition() {
        if deadline.is_some_and(|deadline| Instant::now() > deadline) {
            return false;
        }
        thread::sleep(Duration::from_millis(1));
    }
    true
}

/// A child subscribed to `source` that panics on `poison` and records every
/// other value it sees.
fn reader(
    child: &Domain,
    source: &Cell<i64, hyphae::CellMutable>,
    poison: i64,
) -> (Arc<AtomicI64>, hyphae::SubscriptionGuard) {
    let seen = Arc::new(AtomicI64::new(0));
    let sink = Arc::clone(&seen);
    let guard = child.enter(|| {
        source.subscribe(move |signal| {
            if let Signal::Value(value) = signal {
                assert!(**value != poison, "poisoned value {poison}");
                sink.store(**value, Ordering::SeqCst);
            }
        })
    });
    (seen, guard)
}

#[test]
fn a_panicking_hand_off_delivery_fails_the_child_with_its_cause() {
    let _serial = ROOT_TICK_SERIAL.lock();
    let child = Domain::child();
    let source = Cell::new(0_i64);
    let (seen, _guard) = reader(&child, &source, 2);
    source.set(1);
    child.flush();
    assert_eq!(
        seen.load(Ordering::SeqCst),
        1,
        "the child applies root's writes"
    );
    assert!(child.failure().is_none(), "a healthy child has no failure");

    source.set(2);
    assert!(
        wait_until(|| child.is_closed()),
        "the child closes once its propagation panicked"
    );
    let failure = child.failure().expect("the child records why it failed");
    assert!(
        failure.contains("poisoned value 2"),
        "the failure names the panic: {failure}"
    );

    for value in 3..100 {
        source.set(value);
    }
    child.flush();
    assert_eq!(
        seen.load(Ordering::SeqCst),
        1,
        "a failed child applies nothing after the panic"
    );
    assert_eq!(
        child.pending_hand_offs(),
        0,
        "nothing queues up behind a failed child"
    );
}

#[test]
fn a_panicking_hand_off_closure_fails_the_child() {
    let _serial = ROOT_TICK_SERIAL.lock();
    let child = Domain::child();
    child.hand_off(|| panic!("closure broke"));
    assert!(
        wait_until(|| child.is_closed()),
        "the child closes once a hand-off closure panicked"
    );
    assert!(
        child
            .failure()
            .is_some_and(|failure| failure.contains("closure broke")),
        "the failure names the panic"
    );
}

#[test]
fn a_panic_in_a_child_cascade_on_the_writing_thread_fails_the_child() {
    let _serial = ROOT_TICK_SERIAL.lock();
    let child = Domain::child();
    let (cell, _guard) = child.enter(|| {
        let cell = Cell::new(0_i64);
        let guard = cell.subscribe(|signal| {
            if let Signal::Value(value) = signal {
                assert!(**value != 7, "cascade broke");
            }
        });
        (cell, guard)
    });
    let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| cell.set(7)));
    assert!(
        unwound.is_err() || child.failure().is_some(),
        "the writer sees the panic or the child recorded it"
    );
    assert!(
        wait_until(|| child.is_closed()),
        "the child closes once its cascade panicked"
    );
    assert!(
        child.failure().is_some_and(|failure| !failure.is_empty()),
        "the failure is recorded"
    );
}

#[test]
fn a_child_that_never_panics_never_fails_and_root_has_no_failure() {
    let _serial = ROOT_TICK_SERIAL.lock();
    let child = Domain::child();
    let source = Cell::new(0_i64);
    let (seen, _guard) = reader(&child, &source, i64::MIN);
    for value in 1..50 {
        source.set(value);
    }
    child.flush();
    assert_eq!(seen.load(Ordering::SeqCst), 49, "every write arrives");
    assert!(child.failure().is_none(), "nothing failed");
    assert!(!child.is_closed(), "the child keeps running");
    assert!(
        Domain::root().failure().is_none(),
        "root has no failed state"
    );
}
