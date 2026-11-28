//! Writes across a domain boundary, timers owned by a child, and the life of
//! a child domain.
//!
//! A thread running one domain's propagation writes only that domain. The one
//! crossing is explicit and points from root to a child: `Domain::hand_off`.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::{self, ThreadId},
    time::{Duration, Instant},
};

mod domain_support;

use domain_support::{DEADLINE, Gate, Log, ROOT_TICK_SERIAL, log_values};
use hyphae::{
    Cell, CellMutable, DelayExt, DepNode, Gettable, MapExt, Materialize, Mutable, Signal, Source,
    Watchable, batch, interval, scheduler::Domain,
};
use parking_lot::Mutex;

fn panics<R>(run: impl FnOnce() -> R) -> bool {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(run)).is_err()
}

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

/// A cell in `domain` whose subscriber writes `target` on every change.
fn writer_into(
    domain: &Domain,
    target: &Cell<i64, CellMutable>,
) -> (Cell<i64, CellMutable>, hyphae::SubscriptionGuard) {
    domain.enter(|| {
        let trigger = Cell::new(0i64);
        let target = target.clone();
        let armed = AtomicBool::new(false);
        let guard = trigger.subscribe(move |signal| {
            // The subscribe-time replay is not a propagation.
            if armed.swap(true, Ordering::SeqCst)
                && let Signal::Value(value) = signal
            {
                target.set(**value);
            }
        });
        (trigger, guard)
    })
}

// (d) writes across the boundary --------------------------------------------

#[test]
fn a_child_propagation_cannot_write_root_or_a_sibling() {
    let _serial = ROOT_TICK_SERIAL.lock();
    let child = Domain::child();
    let sibling = Domain::child();
    let root_cell = Cell::new(0i64);
    let sibling_cell = sibling.enter(|| Cell::new(0i64));

    let (to_root, _root_guard) = writer_into(&child, &root_cell);
    let (to_sibling, _sibling_guard) = writer_into(&child, &sibling_cell);

    assert!(
        panics(|| to_root.set(1)),
        "an eager child write reached root"
    );
    assert!(panics(|| child.batch(|| to_root.set(2))));
    assert!(panics(|| to_sibling.set(1)));
    assert!(panics(|| child.batch(|| to_sibling.set(2))));
    assert_eq!(root_cell.get(), 0);
    assert_eq!(sibling_cell.get(), 0);

    // The rejected callback's own cell took its write, and a child callback
    // writes the child's cells freely.
    assert_eq!(to_root.get(), 2);
    let local = child.enter(|| Cell::new(0i64));
    let (to_local, _local_guard) = writer_into(&child, &local);
    to_local.set(9);
    assert_eq!(local.get(), 9);
}

#[test]
fn a_root_propagation_cannot_write_a_child_inline() {
    let _serial = ROOT_TICK_SERIAL.lock();
    let child = Domain::child();
    let child_cell = child.enter(|| Cell::new(0i64));
    let (trigger, _guard) = writer_into(&Domain::root(), &child_cell);

    assert!(panics(|| trigger.set(1)));
    assert!(panics(|| batch(|| trigger.set(2))));
    assert_eq!(child_cell.get(), 0);

    // A thread outside every propagation writes any domain, on that domain's
    // tick.
    child_cell.set(5);
    assert_eq!(child_cell.get(), 5);
}

#[test]
fn an_explicit_hand_off_runs_on_the_child_in_root_order() {
    let _serial = ROOT_TICK_SERIAL.lock();
    let child = Domain::child();
    let root_cell = Cell::new(0i64);
    let order: Log<(&'static str, i64)> = Log::default();
    let ran_on: Log<(ThreadId, Domain)> = Log::default();

    let _reader = child.enter(|| {
        let order = Arc::clone(&order);
        root_cell.subscribe(move |signal| {
            if let Signal::Value(value) = signal {
                order.lock().push(("delivery", **value));
            }
        })
    });
    let hand_off = |value: i64| {
        let order = Arc::clone(&order);
        let ran_on = Arc::clone(&ran_on);
        child.hand_off(move || {
            ran_on
                .lock()
                .push((thread::current().id(), Domain::current()));
            order.lock().push(("hand-off", value));
        });
    };

    // Everything below queues behind a parked unit, so the order asserted is
    // the order of the queue and not of arrival at an idle worker.
    let gate = Arc::new(Gate::default());
    gate.arm();
    child.hand_off({
        let gate = Arc::clone(&gate);
        move || gate.pass()
    });
    assert!(gate.wait_entered());
    order.lock().clear();

    root_cell.set(1);
    hand_off(1);
    batch(|| root_cell.set(2));
    hand_off(2);
    root_cell.set(3);
    assert_eq!(child.pending_hand_offs(), 6);
    gate.open();
    child.flush();

    assert_eq!(
        *order.lock(),
        vec![
            ("delivery", 1),
            ("hand-off", 1),
            ("delivery", 2),
            ("hand-off", 2),
            ("delivery", 3)
        ]
    );

    // From inside root's propagation, a hand-off is the way into a child.
    let _crossing = {
        let child = Domain::clone(&child);
        let order = Arc::clone(&order);
        let ran_on = Arc::clone(&ran_on);
        let armed = AtomicBool::new(false);
        root_cell.subscribe(move |signal| {
            if armed.swap(true, Ordering::SeqCst)
                && let Signal::Value(value) = signal
            {
                let value = **value;
                let order = Arc::clone(&order);
                let ran_on = Arc::clone(&ran_on);
                child.hand_off(move || {
                    ran_on
                        .lock()
                        .push((thread::current().id(), Domain::current()));
                    order.lock().push(("crossing", value));
                });
            }
        })
    };
    root_cell.set(4);
    batch(|| root_cell.set(5));
    child.flush();
    let crossings: Vec<i64> = order
        .lock()
        .iter()
        .filter(|(kind, _)| *kind == "crossing")
        .map(|(_, value)| *value)
        .collect();
    assert_eq!(crossings, vec![4, 5]);

    let caller = thread::current().id();
    let ran_on = ran_on.lock().clone();
    assert_eq!(ran_on.len(), 4);
    assert!(ran_on.iter().all(|(thread, _)| *thread != caller));
    assert!(ran_on.iter().all(|(_, domain)| *domain == child));

    // A child cannot hand work to a sibling or to root.
    let sibling = Domain::child();
    let (trigger, _guard) = child.enter(|| {
        let trigger = Cell::new(0i64);
        let sibling = sibling.clone();
        let armed = AtomicBool::new(false);
        let guard = trigger.subscribe(move |_| {
            if armed.swap(true, Ordering::SeqCst) {
                sibling.hand_off(|| {});
            }
        });
        (trigger, guard)
    });
    assert!(panics(|| trigger.set(1)));
}

#[test]
fn a_root_source_hands_each_emit_to_its_child_subscribers() {
    let _serial = ROOT_TICK_SERIAL.lock();
    let source = Source::<i64>::new();
    let child = Domain::child();
    let seen: Log<i64> = Log::default();
    let threads: Log<ThreadId> = Log::default();
    let root_seen: Log<i64> = Log::default();

    let _root_guard = source.subscribe(log_values(&root_seen));
    let _guard = child.enter(|| {
        let record = log_values(&seen);
        let threads = Arc::clone(&threads);
        source.subscribe(move |signal| {
            threads.lock().push(thread::current().id());
            record(signal);
        })
    });

    for value in 1..=50 {
        source.emit(value);
    }
    child.flush();
    assert_eq!(*seen.lock(), *root_seen.lock());
    assert_eq!(seen.lock().len(), 50);
    let writer = thread::current().id();
    assert!(threads.lock().iter().all(|id| *id != writer));

    // A source born in a child emits inside the child, and a child
    // propagation cannot emit a root source.
    let local = child.enter(Source::<i64>::new);
    let local_seen: Log<i64> = Log::default();
    let _local_guard = child.enter(|| local.subscribe(log_values(&local_seen)));
    local.emit(3);
    assert_eq!(*local_seen.lock(), vec![3]);

    let _bad = child.enter(|| local.subscribe(move |_| source.emit(99)));
    assert!(panics(|| local.emit(4)));
    child.flush();
    assert_eq!(seen.lock().len(), 50);
}

// timers ---------------------------------------------------------------------

#[test]
fn a_childs_timer_fires_on_its_own_tick_and_delays_no_other_domain() {
    let _serial = ROOT_TICK_SERIAL.lock();
    let slow = Domain::child();
    let other = Domain::child();
    let gate = Arc::new(Gate::default());
    let slow_threads: Log<ThreadId> = Log::default();

    let (slow_ticks, _slow_guard) = slow.enter(|| {
        let ticks = interval(Duration::from_millis(5));
        let gate = Arc::clone(&gate);
        let threads = Arc::clone(&slow_threads);
        let armed = AtomicBool::new(false);
        let guard = ticks.subscribe(move |_| {
            if armed.swap(true, Ordering::SeqCst) {
                threads.lock().push(thread::current().id());
                gate.pass();
            }
        });
        (ticks, guard)
    });
    gate.arm();
    assert!(gate.wait_entered(), "the slow child's timer never fired");

    // While that timer callback is parked, timers of a sibling and of root
    // still fire, and a delayed value still arrives.
    let other_ticks = other.enter(|| interval(Duration::from_millis(5)));
    let root_ticks = interval(Duration::from_millis(5));
    let (delayed_source, delayed) = other.enter(|| {
        let source = Cell::new(0i64);
        let delayed = source.clone().delay(Duration::from_millis(5)).materialize();
        (source, delayed)
    });
    delayed_source.set(7);
    assert!(wait_until(|| other_ticks.get() >= 3));
    assert!(wait_until(|| root_ticks.get() >= 3));
    assert!(wait_until(|| delayed.get() == 7));
    assert!(!gate.is_open());
    // Ticks the parked child is late for are skipped, not queued.
    assert!(slow.pending_hand_offs() <= 1);

    gate.open();
    assert!(wait_until(|| slow_threads.lock().len() >= 3));
    let test_thread = thread::current().id();
    let threads = slow_threads.lock().clone();
    let first = threads.first().copied();
    assert!(threads.iter().all(|id| Some(*id) == first));
    assert_ne!(first, Some(test_thread));
    assert_eq!(slow_ticks.domain(), slow);
}

// lifecycle ------------------------------------------------------------------

struct ExitFlag(Arc<AtomicBool>);

impl Drop for ExitFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

thread_local! {
    static EXIT_FLAG: Mutex<Option<ExitFlag>> = const { Mutex::new(None) };
}

#[test]
fn dropping_a_child_stops_its_worker_and_releases_its_root_subscriptions() {
    let _serial = ROOT_TICK_SERIAL.lock();
    let root_cell = Cell::new(0i64);
    let source = Source::<i64>::new();
    let child = Domain::child();
    let deliveries = Arc::new(AtomicU64::new(0));
    let cell_deliveries = Arc::new(AtomicU64::new(0));
    let worker_exited = Arc::new(AtomicBool::new(false));
    let held = Arc::new(());

    let (derived, guards) = child.enter(|| {
        let derived = root_cell.clone().map(|value| *value).materialize();
        let count = Arc::clone(&cell_deliveries);
        let exited = Arc::clone(&worker_exited);
        let held = Arc::clone(&held);
        let cell_guard = root_cell.subscribe(move |_| {
            std::hint::black_box(&held);
            count.fetch_add(1, Ordering::SeqCst);
            // Runs on the worker for every delivery after the replay, and
            // leaves a flag the worker thread sets when it exits.
            EXIT_FLAG.with(|flag| {
                flag.lock()
                    .get_or_insert_with(|| ExitFlag(Arc::clone(&exited)));
            });
        });
        let count = Arc::clone(&deliveries);
        let source_guard = source.subscribe(move |_| {
            count.fetch_add(1, Ordering::SeqCst);
        });
        (derived, vec![cell_guard, source_guard])
    });
    // The replay ran on this thread: clear its flag so only the worker's counts.
    EXIT_FLAG.with(|flag| std::mem::forget(flag.lock().take()));
    worker_exited.store(false, Ordering::SeqCst);

    root_cell.set(1);
    source.emit(1);
    child.flush();
    assert_eq!(derived.get(), 1);
    assert_eq!(cell_deliveries.swap(0, Ordering::SeqCst), 2);
    assert_eq!(deliveries.swap(0, Ordering::SeqCst), 1);
    assert_eq!(root_cell.subscriber_count(), 2);
    assert_eq!(source.subscriber_count(), 1);
    assert_eq!(Arc::strong_count(&held), 2);

    // Park the worker inside a unit and queue more behind it.
    let gate = Arc::new(Gate::default());
    gate.arm();
    child.hand_off({
        let gate = Arc::clone(&gate);
        move || gate.pass()
    });
    assert!(gate.wait_entered());
    root_cell.set(2);
    root_cell.set(3);
    // The parked unit, and one unit per subscriber for each of the two writes.
    assert_eq!(child.pending_hand_offs(), 5);

    let view = derived.domain();
    let closer = thread::spawn(move || drop(child));
    // The subscriptions and the queued units go while the worker is still
    // inside its unit, and root no longer holds the child's closures.
    assert!(wait_until(|| view.is_closed()));
    assert!(wait_until(|| root_cell.subscriber_count() == 0));
    assert!(wait_until(|| source.subscriber_count() == 0));
    assert!(wait_until(|| Arc::strong_count(&held) == 1));
    assert!(!worker_exited.load(Ordering::SeqCst));
    gate.open();
    assert!(closer.join().is_ok());
    assert!(worker_exited.load(Ordering::SeqCst), "the worker is alive");

    // Nothing is delivered afterwards: not what was queued, not later writes.
    root_cell.set(4);
    batch(|| root_cell.set(5));
    source.emit(2);
    let ran = Arc::new(AtomicBool::new(false));
    view.hand_off({
        let ran = Arc::clone(&ran);
        move || ran.store(true, Ordering::SeqCst)
    });
    view.flush();
    assert!(
        !ran.load(Ordering::SeqCst),
        "a closed domain ran a hand-off"
    );
    assert_eq!(deliveries.load(Ordering::SeqCst), 0);
    assert_eq!(cell_deliveries.load(Ordering::SeqCst), 0);
    assert_eq!(view.pending_hand_offs(), 0);
    assert_eq!(derived.get(), 1);

    // The child's cells are still cells.
    let local = view.enter(|| Cell::new(1i64));
    let doubled = view.enter(|| {
        local
            .clone()
            .map(|value| value.saturating_mul(2))
            .materialize()
    });
    local.set(4);
    assert_eq!(doubled.get(), 8);
    drop(guards);
}
