//! A stint ends even when callbacks keep queuing event sets.
//!
//! This test has its own binary because it reads the process-wide
//! `lock_waits` counter to learn that a writer is asleep.

use std::{
    sync::{Arc, mpsc},
    thread::{self, ThreadId},
    time::{Duration, Instant},
};

use hyphae::{Cell, Gettable, Mutable, Signal, Watchable};
use parking_lot::Mutex;

const ROUNDS: u64 = 40;

#[test]
fn a_submit_caller_settles_only_what_was_queued_when_it_took_the_lock() {
    let trigger = Cell::new(0u64);
    let feedback = Cell::new(0u64);
    let other = Cell::new(0u64);
    let ran_on: Arc<Mutex<Vec<ThreadId>>> = Arc::new(Mutex::new(Vec::new()));

    let (ask_next, next_asked) = mpsc::channel::<u64>();
    let (next_queued, wait_next_queued) = mpsc::channel::<()>();
    let (start_sleeper, sleeper_started) = mpsc::channel::<()>();

    thread::scope(|scope| {
        // Queues the next round's write from another thread, so that it sits
        // in the input queue ahead of the feedback write of the round that
        // asked for it.
        let queued_trigger = trigger.clone();
        scope.spawn(move || {
            for round in next_asked {
                hyphae::enqueue(&[], None, || queued_trigger.set(round));
                next_queued.send(()).ok();
            }
        });
        // A writer that sleeps behind the submit caller's settle.
        let slept_on = other.clone();
        scope.spawn(move || {
            sleeper_started.recv().ok();
            slept_on.set(1);
        });

        let (log, target) = (ran_on.clone(), feedback.clone());
        let (ask_next, wait_next_queued, start_sleeper) = (
            Mutex::new(ask_next),
            Mutex::new(wait_next_queued),
            Mutex::new(start_sleeper),
        );
        let guard = trigger.subscribe(move |signal| {
            let Signal::Value(round) = signal else { return };
            let round = **round;
            if round == 0 {
                return;
            }
            log.lock().push(thread::current().id());
            if round == 1 {
                let before = hyphae::settle::stats().lock_waits;
                start_sleeper.lock().send(()).ok();
                let deadline = Instant::now() + Duration::from_secs(10);
                while hyphae::settle::stats().lock_waits == before {
                    assert!(Instant::now() < deadline, "the writer never slept");
                    thread::yield_now();
                }
            }
            if round < ROUNDS {
                ask_next.lock().send(round + 1).ok();
                wait_next_queued.lock().recv().ok();
            }
            // An input cell, so this write is deferred to a later settle.
            target.set(round);
        });

        hyphae::submit(&[], None, || trigger.set(1));
        let caller = thread::current().id();
        let rounds_on_caller = ran_on.lock().iter().filter(|id| **id == caller).count();
        assert_eq!(
            rounds_on_caller, 1,
            "the submit caller kept settling rounds that were queued after it took the lock"
        );

        let deadline = Instant::now() + Duration::from_secs(10);
        while feedback.get() != ROUNDS {
            assert!(Instant::now() < deadline, "the queued rounds never settled");
            hyphae::flush();
        }
        assert_eq!(ran_on.lock().len(), usize::try_from(ROUNDS).unwrap_or(0));
        assert_eq!(other.get(), 1);
        // Ends the queuing thread, which runs until the callback drops its sender.
        drop(guard);
    });
}
