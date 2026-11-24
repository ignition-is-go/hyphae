//! Atomicity and ordering of scheduling domains under concurrent writers.
//!
//! A batch is atomic to observers: no observer sees some of its writes
//! without the rest, whichever thread drains the tick. The same holds for one
//! hand-off unit inside a child. `flush` covers every root write that returned
//! before it was called, and nothing a root drain produced for a child is
//! stranded or overtaken.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::Duration,
};

mod domain_support;

use domain_support::{Gate, Log, ROOT_TICK_SERIAL, log_values};
use hyphae::{
    Cell, CellImmutable, CellMutable, Gettable, JoinExt, MapExt, Materialize, Mutable, Watchable,
    batch, scheduler::Domain,
};

type Pair = Cell<(i64, i64), CellImmutable>;

fn pair_of(left: &Cell<i64, CellMutable>, right: &Cell<i64, CellMutable>) -> Pair {
    left.clone().join(right.clone()).materialize()
}

/// Park a thread inside an open batch on `domain` after it wrote `first` and
/// before it writes `second`, close an unrelated batch of the same domain on
/// this thread, and assert the observer saw neither write.
fn assert_open_batch_is_invisible(domain: &Domain) {
    let (first, second, unrelated, pair) = domain.enter(|| {
        let first = Cell::new(0i64);
        let second = Cell::new(0i64);
        let pair = pair_of(&first, &second);
        (first, second, Cell::new(0i64), pair)
    });
    let seen: Log<(i64, i64)> = Log::default();
    let _guard = domain.enter(|| pair.subscribe(log_values(&seen)));
    let gate = Arc::new(Gate::default());
    gate.arm();

    let writer = {
        let domain = domain.clone();
        let gate = Arc::clone(&gate);
        thread::spawn(move || {
            domain.batch(|| {
                first.set(1);
                gate.pass();
                second.set(1);
            });
        })
    };
    assert!(gate.wait_entered(), "the writer never opened its batch");

    domain.batch(|| unrelated.set(1));
    assert_eq!(pair.get(), (0, 0), "an open batch leaked a write");

    gate.open();
    assert!(writer.join().is_ok());
    assert_eq!(pair.get(), (1, 1));
    assert_eq!(*seen.lock(), vec![(0, 0), (1, 1)]);
}

#[test]
fn a_root_batch_is_invisible_until_it_closes() {
    let _serial = ROOT_TICK_SERIAL.lock();
    assert_open_batch_is_invisible(&Domain::root());
}

#[test]
fn a_child_batch_is_invisible_until_it_closes() {
    let _serial = ROOT_TICK_SERIAL.lock();
    assert_open_batch_is_invisible(&Domain::child());
}

#[test]
fn a_hand_off_unit_is_not_torn_by_a_concurrent_child_batch() {
    let _serial = ROOT_TICK_SERIAL.lock();
    let first = Cell::new(0i64);
    let second = Cell::new(0i64);
    let child = Domain::child();
    let gate = Arc::new(Gate::default());

    // Both root cells are read through a map that parks on its second armed
    // call, so the worker parks after applying the first delivery of the unit
    // and before the second.
    let armed_calls = Arc::new(AtomicU64::new(0));
    let (pair, local) = child.enter(|| {
        let read = |cell: &Cell<i64, CellMutable>| {
            let gate = Arc::clone(&gate);
            let armed_calls = Arc::clone(&armed_calls);
            cell.clone()
                .map(move |value| {
                    if gate.is_armed() && armed_calls.fetch_add(1, Ordering::SeqCst) == 1 {
                        gate.pass();
                    }
                    *value
                })
                .materialize()
        };
        let pair = read(&first).join(read(&second)).materialize();
        (pair, Cell::new(0i64))
    });
    let seen: Log<(i64, i64)> = Log::default();
    let _guard = child.enter(|| pair.subscribe(log_values(&seen)));

    gate.arm();
    batch(|| {
        first.set(1);
        second.set(1);
    });
    assert!(gate.wait_entered(), "the unit never reached the child");

    child.batch(|| local.set(1));
    assert_eq!(pair.get(), (0, 0), "half a hand-off unit was drained");

    gate.open();
    child.flush();
    assert_eq!(*seen.lock(), vec![(0, 0), (1, 1)]);
}

#[test]
fn flush_waits_for_a_root_drain_that_holds_an_earlier_write() {
    let _serial = ROOT_TICK_SERIAL.lock();
    let slow_source = Cell::new(0i64);
    let written = Cell::new(0i64);
    let gate = Arc::new(Gate::default());
    let _slow = {
        let gate = Arc::clone(&gate);
        slow_source
            .clone()
            .map(move |value| {
                gate.pass();
                *value
            })
            .materialize()
    };
    let child = Domain::child();
    let seen: Log<i64> = Log::default();
    let _guard = child.enter(|| written.subscribe(log_values(&seen)));

    gate.arm();
    let drainer = thread::spawn(move || batch(|| slow_source.set(1)));
    assert!(gate.wait_entered(), "the root drain never started");

    // The root tick is draining, so this write joins that drain and returns.
    written.set(7);

    let returned = Arc::new(AtomicBool::new(false));
    let flusher = {
        let returned = Arc::clone(&returned);
        let seen = Arc::clone(&seen);
        thread::spawn(move || {
            child.flush();
            returned.store(true, Ordering::SeqCst);
            seen.lock().clone()
        })
    };
    thread::sleep(Duration::from_millis(100));
    assert!(
        !returned.load(Ordering::SeqCst),
        "flush returned while a root drain still held the write"
    );

    gate.open();
    assert!(drainer.join().is_ok());
    assert!(matches!(flusher.join(), Ok(seen) if seen == vec![0, 7]));
}

#[test]
fn a_root_drain_that_hands_its_claim_over_strands_nothing() {
    let _serial = ROOT_TICK_SERIAL.lock();
    let source = Cell::new(0i64);
    // 64 waves is one drain stint: the drain below gives up its claim exactly
    // when its queue runs dry, because another batch is still open.
    let mut tail = source.clone().map(|value| *value).materialize();
    for _ in 0..62 {
        tail = tail.map(|value| *value).materialize();
    }
    let child = Domain::child();
    let seen: Log<i64> = Log::default();
    let _guard = child.enter(|| tail.subscribe(log_values(&seen)));

    let gate = Arc::new(Gate::default());
    gate.arm();
    let holder = {
        let gate = Arc::clone(&gate);
        thread::spawn(move || batch(|| gate.pass()))
    };
    assert!(gate.wait_entered(), "the other batch never opened");

    batch(|| source.set(1));
    assert_eq!(tail.get(), 1);
    gate.open();
    assert!(holder.join().is_ok());

    child.flush();
    assert_eq!(*seen.lock(), vec![0, 1], "a delivery was stranded in root");

    source.set(2);
    child.flush();
    assert_eq!(*seen.lock(), vec![0, 1, 2]);
}

#[test]
fn a_subscription_racing_root_writes_never_goes_backwards() {
    let _serial = ROOT_TICK_SERIAL.lock();
    let source = Cell::new(0i64);
    let child = Domain::child();
    let stop = Arc::new(AtomicBool::new(false));

    let writer = {
        let source = source.clone();
        let stop = Arc::clone(&stop);
        thread::spawn(move || {
            let mut next = 0i64;
            while !stop.load(Ordering::Relaxed) {
                next = next.saturating_add(1);
                source.set(next);
            }
        })
    };

    let mut logs: Vec<Log<i64>> = Vec::new();
    let mut guards = Vec::new();
    for _ in 0..200_000 {
        let seen: Log<i64> = Log::default();
        guards.push(child.enter(|| source.subscribe(log_values(&seen))));
        logs.push(seen);
        if guards.len() > 64 {
            guards.clear();
        }
    }
    stop.store(true, Ordering::Relaxed);
    assert!(writer.join().is_ok());
    child.flush();

    for seen in logs {
        let seen = seen.lock().clone();
        assert!(
            seen.windows(2).all(|pair| matches!(pair, [a, b] if a <= b)),
            "a delivery older than the seed arrived after it: {seen:?}"
        );
    }
}
