//! Behavior of a settle that fires equal-height nodes on several threads.
//!
//! The wave switches are process-wide, so this file has its own binary and
//! its tests take turns.

use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use hyphae::{
    Cell, CellImmutable, CellMutable, Epoch, Gettable, MapExt, Materialize, MergeExt, Mutable,
    Signal, Source, SubscriptionGuard, Watchable, batch_with, enqueue, flush, settle, submit,
};
use parking_lot::{Mutex, MutexGuard};

static TURN: Mutex<()> = Mutex::new(());

/// Fire every wave on `workers` threads until the returned guard drops. Zero
/// fires on the holder.
fn with_workers(workers: usize) -> MutexGuard<'static, ()> {
    let turn = TURN.lock();
    settle::set_wave_min(1);
    settle::set_workers(workers);
    turn
}

/// Runs `callback` for each value `cell` delivers after subscription.
fn on_change<T: hyphae::CellValue>(
    cell: &impl Watchable<T>,
    callback: impl Fn(&T) + Send + Sync + 'static,
) -> SubscriptionGuard {
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

/// `width` cells of one height, each derived from `root`.
fn leaves(root: &Cell<u64, CellMutable>, width: u64) -> Vec<Cell<u64, CellImmutable>> {
    (0..width)
        .map(|index| {
            root.clone()
                .map(move |value| value.wrapping_mul(1_000).wrapping_add(index))
                .materialize()
        })
        .collect()
}

/// Busy for longer the earlier the leaf, so that under waves a later leaf
/// finishes before an earlier one.
fn stagger(index: u64, width: u64) {
    let busy = Duration::from_micros(width.saturating_sub(index).saturating_mul(20));
    let started = Instant::now();
    while started.elapsed() < busy {
        std::hint::spin_loop();
    }
}

#[test]
fn nodes_of_one_height_fire_at_the_same_time() {
    let _turn = with_workers(4);
    let root = Cell::new(0u64);
    let leaves = leaves(&root, 8);
    let inside = Arc::new(AtomicUsize::new(0));
    let overlapped = Arc::new(AtomicBool::new(false));
    let fired = Arc::new(AtomicUsize::new(0));
    let _guards: Vec<_> = leaves
        .iter()
        .map(|leaf| {
            let (inside, overlapped, fired) = (inside.clone(), overlapped.clone(), fired.clone());
            on_change(leaf, move |_| {
                fired.fetch_add(1, Ordering::SeqCst);
                inside.fetch_add(1, Ordering::SeqCst);
                let started = Instant::now();
                while !overlapped.load(Ordering::SeqCst)
                    && started.elapsed() < Duration::from_secs(5)
                {
                    if inside.load(Ordering::SeqCst) >= 2 {
                        overlapped.store(true, Ordering::SeqCst);
                    }
                    thread::yield_now();
                }
                inside.fetch_sub(1, Ordering::SeqCst);
            })
        })
        .collect();

    root.set(1);

    assert!(
        overlapped.load(Ordering::SeqCst),
        "two callbacks of one wave must be inside at once"
    );
    assert_eq!(fired.load(Ordering::SeqCst), 8, "each node fires once");
}

#[test]
fn one_worker_runs_every_callback_off_the_writer_thread() {
    let _turn = with_workers(1);
    let root = Cell::new(0u64);
    let leaves = leaves(&root, 4);
    let ran_on = Arc::new(Mutex::new(Vec::new()));
    let _guards: Vec<_> = leaves
        .iter()
        .map(|leaf| {
            let ran_on = ran_on.clone();
            on_change(leaf, move |_| ran_on.lock().push(thread::current().id()))
        })
        .collect();

    root.set(1);

    let ran_on = ran_on.lock().clone();
    assert_eq!(ran_on.len(), 4);
    assert!(
        ran_on.iter().all(|id| *id != thread::current().id()),
        "no callback may run on the writer"
    );
    assert_eq!(leaves.first().map(Gettable::get), Some(1_000));
}

/// The same write is settled on the holder and then on several threads, on
/// one graph: subscribers of a cell run in an order that differs from graph
/// to graph, so only one graph can say what "the order of one thread" is.
const WORKERS: [usize; 2] = [4, 16];

#[test]
fn sets_queued_during_a_wave_settle_in_the_order_of_one_thread() {
    const WIDTH: u64 = 48;
    let _turn = with_workers(0);
    let root = Cell::new(0u64);
    let sink = Source::<u64>::new();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    let _sink_guard = sink.subscribe(move |signal| {
        if let Signal::Value(value) = signal {
            log.lock().push(**value % 1_000);
        }
    });
    // Two levels, so the second wave's order comes from the merge of the
    // first wave's frontier entries.
    let first = leaves(&root, WIDTH);
    let second: Vec<_> = first
        .iter()
        .map(|leaf| leaf.clone().map(|value| *value).materialize())
        .collect();
    let _guards: Vec<_> = second
        .iter()
        .zip(0..WIDTH)
        .map(|(leaf, index)| {
            let sink = sink.clone();
            on_change(leaf, move |value| {
                stagger(index, WIDTH);
                sink.emit(*value);
            })
        })
        .collect();
    let mut write = 0u64;
    let mut settle_once = || {
        write = write.saturating_add(1);
        root.set(write);
        std::mem::take(&mut *seen.lock())
    };

    let expected = settle_once();
    assert_eq!(
        expected.len(),
        usize::try_from(WIDTH).unwrap_or(usize::MAX),
        "a waiting writer returns after the feedback of its write has settled"
    );
    for workers in WORKERS {
        settle::set_workers(workers);
        for _ in 0..10 {
            assert_eq!(settle_once(), expected, "{workers} workers");
        }
    }
}

#[test]
fn values_that_reach_one_node_from_a_wave_keep_the_order_of_one_thread() {
    const WIDTH: u64 = 16;
    let _turn = with_workers(0);
    let root = Cell::new(0u64);
    // A tree of merges whose two inputs always have one height, so both
    // fire in one wave and push into the merge from two threads.
    let mut level: Vec<_> = leaves(&root, WIDTH)
        .into_iter()
        .zip(0..WIDTH)
        .map(|(leaf, index)| {
            leaf.map(move |value| {
                stagger(index, WIDTH);
                *value
            })
            .materialize()
        })
        .collect();
    while level.len() > 1 {
        level = level
            .chunks(2)
            .filter_map(|pair| match pair {
                [left, right] => Some(left.clone().merge(right.clone()).materialize()),
                [last] => Some(last.clone()),
                _ => None,
            })
            .collect();
    }
    let Some(merged) = level.pop() else { return };
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    let _guard = on_change(&merged, move |value| log.lock().push(*value % 1_000));
    let mut write = 0u64;
    let mut settle_once = || {
        write = write.saturating_add(1);
        root.set(write);
        std::mem::take(&mut *seen.lock())
    };

    let expected = settle_once();
    assert_eq!(expected.len(), usize::try_from(WIDTH).unwrap_or(usize::MAX));
    for workers in WORKERS {
        settle::set_workers(workers);
        for _ in 0..10 {
            assert_eq!(settle_once(), expected, "{workers} workers");
        }
    }
}

#[test]
#[allow(clippy::panic)]
fn the_first_panic_in_the_order_of_one_thread_reaches_the_writer() {
    const WIDTH: u64 = 16;
    let _turn = with_workers(0);
    let root = Cell::new(0u64);
    let leaves = leaves(&root, WIDTH);
    let _guards: Vec<_> = leaves
        .iter()
        .zip(0..WIDTH)
        .map(|(leaf, index)| {
            on_change(leaf, move |_| {
                stagger(index, WIDTH);
                std::panic::panic_any(index);
            })
        })
        .collect();
    let mut write = 0u64;
    let mut first_panic = || {
        write = write.saturating_add(1);
        let outcome = catch_unwind(AssertUnwindSafe(|| root.set(write)));
        assert_eq!(root.get(), write, "the settle went on after the panics");
        outcome
            .err()
            .and_then(|payload| payload.downcast::<u64>().ok())
            .map(|index| *index)
    };

    let expected = first_panic();
    assert!(expected.is_some());
    for workers in WORKERS {
        settle::set_workers(workers);
        for _ in 0..5 {
            assert_eq!(first_panic(), expected, "{workers} workers");
        }
    }
}

#[test]
fn the_counters_report_waves() {
    let _turn = with_workers(4);
    let root = Cell::new(0u64);
    let leaves = leaves(&root, 32);
    let _guards: Vec<_> = leaves.iter().map(|leaf| on_change(leaf, |_| {})).collect();
    let before = settle::stats();

    root.set(1);

    let after = settle::stats();
    assert!(after.waves >= before.waves.saturating_add(2));
    assert!(after.parallel_waves > before.parallel_waves);
    assert!(after.widest_wave >= 32);
}

#[test]
fn a_write_and_the_feedback_it_causes_keep_its_epoch_on_every_thread() {
    let _turn = with_workers(4);
    let root = Cell::new(0u64);
    let feedback = Cell::new(0u64);
    let leaves = leaves(&root, 8);
    let in_leaves = Arc::new(Mutex::new(Vec::new()));
    let in_feedback = Arc::new(Mutex::new(Vec::new()));
    let _guards: Vec<_> = leaves
        .iter()
        .map(|leaf| {
            let (log, feedback) = (in_leaves.clone(), feedback.clone());
            on_change(leaf, move |value| {
                log.lock().push(settle::current_epoch());
                // An input cell, so this write is a later event set.
                feedback.set(*value);
            })
        })
        .collect();
    let log = in_feedback.clone();
    let _feedback_guard = on_change(&feedback, move |_| log.lock().push(settle::current_epoch()));
    let epoch = Epoch::next();

    batch_with(&[], Some(epoch), || root.set(1));
    assert_eq!(*in_leaves.lock(), vec![Some(epoch); 8]);
    let caused = std::mem::take(&mut *in_feedback.lock());
    assert!(!caused.is_empty());
    assert!(caused.iter().all(|seen| *seen == Some(epoch)));
    assert_eq!(settle::current_epoch(), None, "no settle is in progress");

    in_leaves.lock().clear();
    root.set(2);
    assert_eq!(*in_leaves.lock(), vec![None; 8], "a plain write");
}

#[test]
fn event_sets_share_a_settle_only_within_one_epoch() {
    let _turn = with_workers(0);
    let cell = Cell::new(0u64);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = seen.clone();
    let _guard = on_change(&cell, move |value| {
        log.lock().push((*value, settle::current_epoch()));
    });
    let (first, second) = (Epoch::next(), Epoch::next());
    let splits = settle::stats().epoch_splits;

    // Sets of one cell that share a settle leave the last value.
    enqueue(&[], None, || cell.set(1));
    enqueue(&[], None, || cell.set(2));
    flush();
    assert_eq!(std::mem::take(&mut *seen.lock()), vec![(2, None)]);
    enqueue(&[], Some(first), || cell.set(3));
    enqueue(&[], Some(first), || cell.set(4));
    flush();
    assert_eq!(std::mem::take(&mut *seen.lock()), vec![(4, Some(first))]);
    assert_eq!(settle::stats().epoch_splits, splits);

    enqueue(&[], Some(first), || cell.set(5));
    enqueue(&[], Some(second), || cell.set(6));
    flush();
    assert_eq!(
        std::mem::take(&mut *seen.lock()),
        vec![(5, Some(first)), (6, Some(second))]
    );
    assert_eq!(settle::stats().epoch_splits, splits.saturating_add(1));
}

#[test]
fn a_scoped_epoch_finishes_after_its_scope_and_its_last_event_set() {
    let _turn = with_workers(4);
    let root = Cell::new(0u64);
    let feedback = Cell::new(0u64);
    let finished = Arc::new(AtomicBool::new(false));
    let leaves = leaves(&root, 8);
    let _guards: Vec<_> = leaves
        .iter()
        .map(|leaf| {
            let feedback = feedback.clone();
            on_change(leaf, move |value| feedback.set(*value))
        })
        .collect();
    let (flag, early) = (finished.clone(), Arc::new(AtomicBool::new(false)));
    let saw_early = early.clone();
    let _feedback_guard = on_change(&feedback, move |_| {
        if flag.load(Ordering::SeqCst) {
            saw_early.store(true, Ordering::SeqCst);
        }
    });

    // The scope outlives the event sets.
    let flag = finished.clone();
    let scope = Epoch::scoped(move || flag.store(true, Ordering::SeqCst));
    batch_with(&[], Some(scope.epoch()), || root.set(1));
    assert!(!finished.load(Ordering::SeqCst), "the scope is still held");
    drop(scope);
    assert!(finished.load(Ordering::SeqCst));
    assert!(
        !early.load(Ordering::SeqCst),
        "the feedback of the write settled before the epoch finished"
    );

    // The event sets outlive the scope.
    finished.store(false, Ordering::SeqCst);
    let flag = finished.clone();
    let scope = Epoch::scoped(move || flag.store(true, Ordering::SeqCst));
    enqueue(&[], Some(scope.epoch()), || root.set(2));
    drop(scope);
    assert!(!finished.load(Ordering::SeqCst), "a set is still queued");
    flush();
    assert!(finished.load(Ordering::SeqCst));
    assert!(!early.load(Ordering::SeqCst));
}

#[test]
fn a_scoped_epoch_outlives_a_scope_dropped_inside_an_outer_batch() {
    let _turn = with_workers(4);
    let root = Cell::new(0u64);
    let feedback = Cell::new(0u64);
    let finished = Arc::new(AtomicBool::new(false));
    let early = Arc::new(AtomicBool::new(false));
    let leaves = leaves(&root, 8);
    let _guards: Vec<_> = leaves
        .iter()
        .map(|leaf| {
            let feedback = feedback.clone();
            on_change(leaf, move |value| feedback.set(*value))
        })
        .collect();
    let (flag, saw_early) = (finished.clone(), early.clone());
    let _feedback_guard = on_change(&feedback, move |_| {
        if flag.load(Ordering::SeqCst) {
            saw_early.store(true, Ordering::SeqCst);
        }
    });

    let flag = finished.clone();
    submit(&[], None, || {
        let scope = Epoch::scoped(move || flag.store(true, Ordering::SeqCst));
        batch_with(&[], Some(scope.epoch()), || root.set(1));
        drop(scope);
        assert!(
            !finished.load(Ordering::SeqCst),
            "the outer batch still holds the write"
        );
    });
    flush();
    assert!(finished.load(Ordering::SeqCst));
    assert!(
        !early.load(Ordering::SeqCst),
        "the feedback of the write settled before the epoch finished"
    );

    // A batch that wrote nothing is never queued and holds nothing open.
    finished.store(false, Ordering::SeqCst);
    let flag = finished.clone();
    let scope = Epoch::scoped(move || flag.store(true, Ordering::SeqCst));
    batch_with(&[], Some(scope.epoch()), || {});
    drop(scope);
    assert!(finished.load(Ordering::SeqCst));
}

#[test]
fn a_thread_knows_whether_it_is_delivering_a_write() {
    let _turn = with_workers(4);
    let root = Cell::new(0u64);
    let leaves = leaves(&root, 8);
    let inside = Arc::new(Mutex::new(Vec::new()));
    let _guards: Vec<_> = leaves
        .iter()
        .map(|leaf| {
            let inside = inside.clone();
            on_change(leaf, move |_| inside.lock().push(settle::is_settling()))
        })
        .collect();

    assert!(!settle::is_settling());
    root.set(1);
    assert_eq!(*inside.lock(), vec![true; 8]);
    assert!(!settle::is_settling());
}

#[test]
fn a_switch_setter_returns_the_value_it_replaced() {
    let _turn = with_workers(3);
    assert_eq!(settle::set_workers(5), 3);
    assert_eq!(settle::set_workers(0), 5);
    assert_eq!(settle::set_wave_min(7), 1);
    assert_eq!(settle::set_wave_min(1), 7);
}
