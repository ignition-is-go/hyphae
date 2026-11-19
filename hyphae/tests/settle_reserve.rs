//! Runs that reserve their cones: disjoint writes settle at the same time,
//! and runs that share a node settle there in arrival order.
//!
//! The wave switches are process-wide, so the tests in this binary take turns.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

use hyphae::{
    Cell, Gettable, JoinExt, MapExt, Materialize, MergeExt, Mutable, SettleKey, Signal,
    SwitchMapExt, Watchable, batch_with, settle, submit,
};
use parking_lot::{Mutex, MutexGuard};

static TURN: Mutex<()> = Mutex::new(());

const PATIENCE: Duration = Duration::from_secs(10);

fn turn() -> MutexGuard<'static, ()> {
    TURN.lock()
}

/// Calls `callback` for each value after the replay on subscribe.
fn on_value<T: hyphae::CellValue>(
    cell: &impl Watchable<T>,
    callback: impl Fn(&T) + Send + Sync + 'static,
) -> hyphae::SubscriptionGuard {
    let first = AtomicBool::new(true);
    cell.subscribe(move |signal| {
        if first.swap(false, Ordering::SeqCst) {
            return;
        }
        if let Signal::Value(value) = signal {
            callback(value);
        }
    })
}

/// Blocks a settle of `cell` in a callback until the returned sender fires.
/// The receiver hears when the callback has started.
fn hold(
    cell: &Cell<u64, hyphae::CellMutable>,
) -> (
    mpsc::Sender<()>,
    mpsc::Receiver<()>,
    hyphae::SubscriptionGuard,
) {
    let (release, released) = mpsc::channel::<()>();
    let (entered, entered_rx) = mpsc::channel::<()>();
    let released = Mutex::new(released);
    let guard = on_value(cell, move |_| {
        let _ = entered.send(());
        let _ = released.lock().recv_timeout(PATIENCE);
    });
    (release, entered_rx, guard)
}

#[test]
fn two_disjoint_writes_settle_at_the_same_time() {
    let _turn = turn();
    let left = Cell::new(0u64);
    let right = Cell::new(0u64);
    // Each callback waits until the other has started. Two settles that ran
    // one after the other would both time out.
    let (left_in, left_seen) = mpsc::channel::<()>();
    let (right_in, right_seen) = mpsc::channel::<()>();
    let left_seen = Mutex::new(left_seen);
    let right_seen = Mutex::new(right_seen);
    let met = Arc::new(Mutex::new(Vec::new()));
    let (left_met, right_met) = (met.clone(), met.clone());
    let _left = on_value(&left, move |_| {
        let _ = left_in.send(());
        left_met
            .lock()
            .push(right_seen.lock().recv_timeout(PATIENCE).is_ok());
    });
    let _right = on_value(&right, move |_| {
        let _ = right_in.send(());
        right_met
            .lock()
            .push(left_seen.lock().recv_timeout(PATIENCE).is_ok());
    });

    thread::scope(|scope| {
        scope.spawn(|| left.set(1));
        scope.spawn(|| right.set(1));
    });
    assert_eq!(
        *met.lock(),
        vec![true, true],
        "the two settles ran one after the other"
    );
}

#[test]
fn a_write_that_shares_a_node_with_a_settle_in_flight_settles_after_it() {
    let _turn = turn();
    let left = Cell::new(0u64);
    let right = Cell::new(0u64);
    let joined = left.clone().join(right.clone()).materialize();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    let _seen = on_value(&joined, move |pair| sink.lock().push(*pair));
    let (release, entered_rx, _held) = hold(&left);

    thread::scope(|scope| {
        scope.spawn(|| left.set(1));
        assert!(entered_rx.recv_timeout(PATIENCE).is_ok());
        // Arrives after the write to `left`, and reaches the join too.
        submit(&[], None, || right.set(1));
        assert_eq!(
            right.get(),
            0,
            "a write that shares a node overtook the run in flight"
        );
        let _ = release.send(());
    });
    settle::flush();
    assert_eq!(*seen.lock(), vec![(1, 0), (1, 1)]);
}

#[test]
fn a_barrier_waits_for_a_settle_in_flight_on_unrelated_nodes() {
    let _turn = turn();
    let busy = Cell::new(0u64);
    let other = Cell::new(0u64);
    let (release, entered_rx, _held) = hold(&busy);

    thread::scope(|scope| {
        scope.spawn(|| busy.set(1));
        assert!(entered_rx.recv_timeout(PATIENCE).is_ok());
        submit(&[SettleKey::Barrier], None, || other.set(1));
        assert_eq!(other.get(), 0, "a barrier settled beside another run");
        let _ = release.send(());
    });
    settle::flush();
    assert_eq!(other.get(), 1);
}

#[test]
fn events_queued_from_two_threads_reach_a_merge_in_the_order_of_one_thread() {
    let _turn = turn();
    for workers in [0, 1, 8] {
        let previous = settle::set_workers(workers);
        let wave_min = settle::set_wave_min(1);
        let first = Cell::new(0u64);
        let second = Cell::new(0u64);
        let merged = first.clone().merge(second.clone()).materialize();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        let _seen = merged.subscribe(move |signal| {
            if let Signal::Value(value) = signal
                && **value != 0
            {
                sink.lock().push(**value);
            }
        });
        let gate = Cell::new(0u64);
        let (release, entered_rx, _held) = hold(&gate);

        thread::scope(|scope| {
            scope.spawn(|| batch_with(&[SettleKey::Barrier], None, || gate.set(1)));
            assert!(entered_rx.recv_timeout(PATIENCE).is_ok());
            // Queued behind the barrier in this order, by two threads in turn.
            // The cones overlap at the merge, so the sets share one run.
            for value in 1..=40u64 {
                let source = if value % 2 == 1 { &first } else { &second };
                thread::scope(|inner| {
                    inner.spawn(|| {
                        submit(&[], None, || source.notify(Signal::Value(Arc::new(value))));
                    });
                });
            }
            let _ = release.send(());
        });
        settle::flush();
        // One thread fires `first`, queued first, before `second`.
        let expected: Vec<u64> = (1..=40u64)
            .filter(|value| value % 2 == 1)
            .chain((1..=40u64).filter(|value| value % 2 == 0))
            .collect();
        assert_eq!(*seen.lock(), expected, "with {workers} workers");
        settle::set_workers(previous);
        settle::set_wave_min(wave_min);
    }
}

#[test]
fn a_switch_map_rewired_while_other_threads_write_its_inners_converges() {
    let _turn = turn();
    for _ in 0..50 {
        let inners: Vec<_> = (0..4).map(|_| Cell::new(0u64)).collect();
        let selector = Cell::new(0usize);
        let picked = inners.clone();
        let output = selector
            .clone()
            .switch_map(move |index| picked.get(*index).cloned().unwrap_or_else(|| Cell::new(0)))
            .materialize();
        let doubled = output.clone().map(|value| value * 2).materialize();
        thread::scope(|scope| {
            for (index, inner) in inners.iter().enumerate() {
                scope.spawn(move || {
                    for value in 1..=50u64 {
                        inner.set(value * 10 + u64::try_from(index).unwrap_or(0));
                    }
                });
            }
            scope.spawn(|| {
                for index in [1usize, 2, 3, 0, 2] {
                    selector.set(index);
                }
            });
        });
        settle::flush();
        let expected = inners.get(2).map_or(0, Gettable::get);
        assert_eq!(
            output.get(),
            expected,
            "the output follows the selected inner"
        );
        assert_eq!(
            doubled.get(),
            expected * 2,
            "the output's dependents settled"
        );
    }
}

#[test]
fn a_write_that_escapes_its_cone_is_counted_and_keeps_node_order() {
    let _turn = turn();
    let input = Cell::new(0u64);
    let other = Cell::new(0u64);
    // `target` sits above `input` without owning a subscription to it, so a
    // run that writes `input` reaches `target` outside its reserved cone.
    let target = other.clone().map(|value| *value).materialize();
    let deep = target.clone().map(|value| *value).materialize();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    let _seen = on_value(&deep, move |value| sink.lock().push(*value));
    let forward = target;
    let _bridge = on_value(&input, move |value| {
        forward.notify(Signal::Value(Arc::new(*value)));
    });

    let before = settle::stats().cone_escapes;
    thread::scope(|scope| {
        scope.spawn(|| {
            for value in 1..=200u64 {
                input.set(value * 2);
            }
        });
        scope.spawn(|| {
            for value in 1..=200u64 {
                other.set(value * 2 + 1);
            }
        });
    });
    settle::flush();
    assert!(
        settle::stats().cone_escapes > before,
        "no write escaped its cone"
    );
    let seen = seen.lock().clone();
    let evens: Vec<u64> = seen
        .iter()
        .copied()
        .filter(|value| value % 2 == 0)
        .collect();
    let odds: Vec<u64> = seen
        .iter()
        .copied()
        .filter(|value| value % 2 == 1)
        .collect();
    assert_eq!(
        evens,
        (1..=200u64).map(|value| value * 2).collect::<Vec<_>>()
    );
    assert!(
        odds.is_sorted(),
        "one writer's values arrived out of order: {odds:?}"
    );
    assert_eq!(odds.last(), Some(&401));
}
