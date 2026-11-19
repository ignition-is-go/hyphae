//! A write that arrives while another thread is mid-drain must not pull that
//! drain back to the bottom of the graph. Restarting from the shallowest
//! pending cell lets a steady stream of source writes overwrite a deep cell's
//! queued update before it ever runs, so the deep cell stops updating while
//! shallow cells keep up.
#![cfg(feature = "scheduler")]

use std::{
    sync::{Arc, mpsc},
    thread,
};

use hyphae::{Cell, Gettable, MapExt, Materialize, Mutable, Signal, Watchable, batch};
use parking_lot::Mutex;

static SCHEDULER_TEST_LOCK: Mutex<()> = Mutex::new(());

/// Holds one pass mid-graph, writes the source again from another thread, and
/// returns every value the deepest cell then saw.
fn deep_values_when_a_write_arrives_mid_pass(window: std::time::Duration) -> Vec<u64> {
    let _serial = SCHEDULER_TEST_LOCK.lock();
    hyphae::scheduler::set_pass_merge_window_for_test(window);

    let source = Cell::new(0u64);
    let (entered_tx, entered_rx) = mpsc::channel::<()>();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let entered_tx = Mutex::new(entered_tx);
    let release_rx = Mutex::new(release_rx);

    // Holds the first propagation mid-graph until the second write is queued.
    let middle = source
        .clone()
        .map(move |value| {
            if *value == 1 {
                let _ = entered_tx.lock().send(());
                let _ = release_rx.lock().recv();
            }
            *value
        })
        .materialize();
    let deep = middle.map(|value| *value).materialize();

    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    let _guard = deep.subscribe(move |signal| {
        if let Signal::Value(value) = signal
            && **value > 0
        {
            sink.lock().push(**value);
        }
    });

    thread::scope(|scope| {
        let first = scope.spawn(|| batch(|| source.set(1)));
        assert!(entered_rx.recv().is_ok(), "first propagation never started");
        batch(|| source.set(2));
        assert!(release_tx.send(()).is_ok(), "first propagation went away");
        assert!(first.join().is_ok(), "first propagation panicked");
    });

    std::mem::take(&mut *seen.lock())
}

#[test]
fn in_flight_propagation_reaches_deep_cells_before_a_later_write_is_applied() {
    assert_eq!(
        deep_values_when_a_write_arrives_mid_pass(std::time::Duration::ZERO),
        vec![1, 2]
    );
}

/// Inside the window a fresh write still joins the pass in flight, so it
/// settles without waiting for that pass to finish.
#[test]
fn a_write_inside_the_merge_window_joins_the_pass_in_flight() {
    assert_eq!(
        deep_values_when_a_write_arrives_mid_pass(std::time::Duration::from_mins(1)),
        vec![2]
    );
}

#[test]
fn stats_count_a_pass_and_the_cells_it_settled() {
    let _serial = SCHEDULER_TEST_LOCK.lock();
    hyphae::scheduler::set_pass_merge_window_for_test(std::time::Duration::ZERO);

    let source = Cell::new(0u64);
    let derived = source.clone().map(|value| *value).materialize();
    let _guard = derived.subscribe(|_| {});

    let before = hyphae::scheduler::stats();
    batch(|| source.set(1));
    let after = hyphae::scheduler::stats();

    assert_eq!(after.passes.saturating_sub(before.passes), 1);
    assert!(
        after.cells_settled.saturating_sub(before.cells_settled) >= 2,
        "the source and its derived cell both settle: {before:?} -> {after:?}"
    );
    assert!(after.drain_nanos > before.drain_nanos);
}

/// Writes to one cell are applied in the order they arrived, whichever pass
/// they land in. A write held for the next pass must not be overtaken by a
/// later write that the pass in flight makes to the same cell, or the older
/// value would be applied last and win.
#[test]
fn a_held_write_is_not_overtaken_by_a_later_write_from_the_pass_in_flight() {
    let _serial = SCHEDULER_TEST_LOCK.lock();
    hyphae::scheduler::set_pass_merge_window_for_test(std::time::Duration::ZERO);

    let source = Cell::new(0u64);
    let target = Cell::new(0u64);
    let (entered_tx, entered_rx) = mpsc::channel::<()>();
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let entered_tx = Mutex::new(entered_tx);
    let release_rx = Mutex::new(release_rx);
    let held = source
        .clone()
        .map(move |value| {
            if *value == 1 {
                let _ = entered_tx.lock().send(());
                let _ = release_rx.lock().recv();
            }
            *value
        })
        .materialize();
    let later = target.clone();
    let _writer = held.subscribe(move |signal| {
        if let Signal::Value(value) = signal
            && **value == 1
        {
            later.set(20);
        }
    });

    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    let _guard = target.subscribe(move |signal| {
        if let Signal::Value(value) = signal
            && **value > 0
        {
            sink.lock().push(**value);
        }
    });

    thread::scope(|scope| {
        let pass = scope.spawn(|| batch(|| source.set(1)));
        assert!(entered_rx.recv().is_ok(), "the held pass never started");
        // Arrives first, from outside the pass, and is held for the next pass.
        batch(|| target.set(10));
        // The pass in flight then writes the same cell.
        assert!(release_tx.send(()).is_ok(), "the held pass went away");
        assert!(pass.join().is_ok(), "the held pass panicked");
    });

    assert_eq!(seen.lock().last(), Some(&20));
    assert_eq!(target.get(), 20);
}
