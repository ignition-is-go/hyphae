//! A batch opens without taking the tick lock, and a nested batch neither
//! opens nor closes anything on the tick. These tests race those paths
//! against closes and drains on other threads and check what they must keep:
//!
//! - a write made inside a thread's own batch stays deferred until that
//!   thread's outermost batch closes, and the batch reaches observers whole;
//! - a write made outside any batch while other threads' batches open and
//!   close is always propagated, by a drain or synchronously, and never left
//!   in the queue with nobody to drain it.

use std::{
    sync::{
        Arc, Barrier, Mutex, PoisonError,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
};

use hyphae::{
    Cell, CellMutable, Gettable, JoinExt, Materialize, Mutable, Signal, SubscriptionGuard,
    Watchable, batch,
};

const THREADS: usize = 6;
const ROUNDS: u64 = 20_000;

/// The tests share root's tick. Run one at a time, so that once a test's
/// threads have joined no other thread can still be draining its writes.
static SERIAL: Mutex<()> = Mutex::new(());

/// Two cells one thread writes together, and what an observer of both saw.
struct Pair {
    a: Cell<u64, CellMutable>,
    b: Cell<u64, CellMutable>,
    last: Arc<AtomicU64>,
    _guard: SubscriptionGuard,
}

fn pair(torn: &Arc<AtomicU64>) -> Pair {
    let a = Cell::new(0);
    let b = Cell::new(0);
    let last = Arc::new(AtomicU64::new(0));
    let (torn, latest) = (Arc::clone(torn), Arc::clone(&last));
    let guard = a
        .clone()
        .join(b.clone())
        .materialize()
        .subscribe(move |signal| {
            if let Signal::Value(pair) = signal {
                if pair.0 != pair.1 {
                    torn.fetch_add(1, Ordering::Relaxed);
                }
                latest.store(pair.0, Ordering::SeqCst);
            }
        });
    Pair {
        a,
        b,
        last,
        _guard: guard,
    }
}

#[test]
fn a_nested_batch_stays_deferred_and_whole_while_other_threads_open_and_close() {
    let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    let torn = Arc::new(AtomicU64::new(0));
    let early = AtomicU64::new(0);
    let pairs: Vec<Pair> = (0..THREADS).map(|_| pair(&torn)).collect();
    let start = Barrier::new(THREADS);
    thread::scope(|scope| {
        for Pair { a, b, .. } in &pairs {
            let (early, start) = (&early, &start);
            scope.spawn(move || {
                start.wait();
                for i in 1..=ROUNDS {
                    batch(|| {
                        batch(|| a.set(i));
                        // The inner close committed nothing: the write is
                        // still staged in this thread's outer batch.
                        if a.get() >= i {
                            early.fetch_add(1, Ordering::Relaxed);
                        }
                        b.set(i);
                    });
                }
            });
        }
    });
    assert_eq!(
        early.load(Ordering::Relaxed),
        0,
        "a batched write ran before its batch closed"
    );
    assert_eq!(
        torn.load(Ordering::Relaxed),
        0,
        "an observer saw half a batch"
    );
    // Every thread that could drain has returned, so each last batch has
    // been delivered.
    for pair in &pairs {
        assert_eq!(
            pair.last.load(Ordering::SeqCst),
            ROUNDS,
            "a batch was never delivered"
        );
    }
}

#[test]
fn an_unbatched_write_racing_batch_opens_is_never_stranded() {
    let _serial = SERIAL.lock().unwrap_or_else(PoisonError::into_inner);
    let source = Cell::<u64, CellMutable>::new(0);
    let last = Arc::new(AtomicU64::new(0));
    let seen = Arc::clone(&last);
    let _guard = source.subscribe(move |signal| {
        if let Signal::Value(value) = signal {
            seen.store(**value, Ordering::SeqCst);
        }
    });
    let stop = AtomicBool::new(false);
    let start = Barrier::new(THREADS + 1);
    thread::scope(|scope| {
        for _ in 0..THREADS {
            let (stop, start) = (&stop, &start);
            scope.spawn(move || {
                let cell = Cell::<u64, CellMutable>::new(0);
                start.wait();
                let mut i = 0;
                while !stop.load(Ordering::Relaxed) {
                    i += 1;
                    batch(|| cell.set(i));
                }
            });
        }
        start.wait();
        // Each write either runs synchronously or is queued while some batch
        // is counted open, and that batch's close drains it.
        for i in 1..=ROUNDS {
            source.set(i);
        }
        stop.store(true, Ordering::Relaxed);
    });
    // Every batch has closed and no thread drains any more.
    assert_eq!(
        last.load(Ordering::SeqCst),
        ROUNDS,
        "a write was left in the tick with nobody to drain it"
    );
}
