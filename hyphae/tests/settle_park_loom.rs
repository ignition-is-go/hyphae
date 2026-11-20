//! Loom model of the park-then-recheck protocol in `settle.rs`
//! (`poke`, `evaluate`, and the wake at the end of `run`).
//!
//! The scheduler code uses std atomics and parking_lot, which loom cannot
//! instrument, so this test restates the protocol step for step over loom
//! types. Keep the two in sync: the claim states, the order "register on
//! the blocker, then re-check it", and the dirty-claim release.

use loom::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU8, Ordering},
};

const IDLE: u8 = 0;
const CLAIMED: u8 = 1;
const DIRTY: u8 = 2;
const NONE: u64 = u64::MAX;

struct Blocker {
    /// The lowest ticket the blocker still holds. Behind a lock, as the
    /// pending list is in `NodeCore`.
    min_pending: Mutex<u64>,
    parked: Mutex<Vec<Arc<Waiter>>>,
}

struct Waiter {
    ticket: u64,
    sched: AtomicU8,
    queued: AtomicBool,
}

fn poke(waiter: &Arc<Waiter>, blocker: &Blocker) {
    loop {
        match waiter.sched.load(Ordering::Acquire) {
            IDLE => {
                if waiter
                    .sched
                    .compare_exchange(IDLE, CLAIMED, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    evaluate(waiter, blocker);
                    return;
                }
                loom::thread::yield_now();
            }
            CLAIMED => {
                if waiter
                    .sched
                    .compare_exchange(CLAIMED, DIRTY, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    return;
                }
                loom::thread::yield_now();
            }
            _ => {
                if waiter
                    .sched
                    .compare_exchange(DIRTY, DIRTY, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    return;
                }
                loom::thread::yield_now();
            }
        }
    }
}

fn evaluate(waiter: &Arc<Waiter>, blocker: &Blocker) {
    loop {
        // A read-modify-write, so this claim follows every poke that
        // marked it dirty.
        waiter.sched.swap(CLAIMED, Ordering::AcqRel);
        if *blocker.min_pending.lock().unwrap() > waiter.ticket {
            // Ready: the claim moves to the ready queue.
            waiter.queued.store(true, Ordering::Release);
            return;
        }
        blocker.parked.lock().unwrap().push(Arc::clone(waiter));
        if *blocker.min_pending.lock().unwrap() > waiter.ticket {
            continue;
        }
        if waiter
            .sched
            .compare_exchange(CLAIMED, IDLE, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            return;
        }
        loom::thread::yield_now();
    }
}

/// The blocker finishes its fire: it no longer holds the ticket, then wakes
/// what parked on it.
fn blocker_fires(blocker: &Blocker) {
    *blocker.min_pending.lock().unwrap() = NONE;
    let parked = std::mem::take(&mut *blocker.parked.lock().unwrap());
    for waiter in parked {
        poke(&waiter, blocker);
    }
}

#[test]
fn a_parked_waiter_is_never_stranded() {
    // Three preemptions cover every interleaving of the two pokes and the
    // fire around the registration; unbounded, the claim spin loops never end.
    let mut model = loom::model::Builder::new();
    model.preemption_bound = Some(3);
    model.check(|| {
        let blocker = Arc::new(Blocker {
            min_pending: Mutex::new(3),
            parked: Mutex::new(Vec::new()),
        });
        let waiter = Arc::new(Waiter {
            ticket: 5,
            sched: AtomicU8::new(IDLE),
            queued: AtomicBool::new(false),
        });
        let firing = {
            let blocker = Arc::clone(&blocker);
            loom::thread::spawn(move || blocker_fires(&blocker))
        };
        let poking = {
            let blocker = Arc::clone(&blocker);
            let waiter = Arc::clone(&waiter);
            loom::thread::spawn(move || poke(&waiter, &blocker))
        };
        poke(&waiter, &blocker);
        firing.join().unwrap();
        poking.join().unwrap();
        assert!(
            waiter.queued.load(Ordering::Acquire),
            "the blocker fired, so the waiter must have been queued to fire"
        );
    });
}
