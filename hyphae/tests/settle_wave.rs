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
    Cell, CellImmutable, CellMutable, Gettable, MapExt, Materialize, MergeExt, Mutable, Signal,
    Source, SubscriptionGuard, Watchable, settle,
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
