//! Semantics of scheduling domains: one root domain, any number of child
//! domains, each with its own propagation tick.
//!
//! The tests are structural. A gate parks a closure inside a wave, and the
//! assertions run while it is parked, so "does not delay" means "completed
//! while the other domain was provably still busy", not "was fast enough".
//! The only clock is a failure deadline on each wait.

use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    thread::{self, ThreadId},
    time::Duration,
};

use hyphae::{
    Cell, CellMutable, DepNode, Gettable, JoinExt, MapExt, Materialize, Mutable, Signal,
    SubscriptionGuard, SwitchMapExt, Watchable, batch,
    scheduler::{Domain, DomainError, no_coalesce},
};
use parking_lot::{Condvar, Mutex};

const DEADLINE: Duration = Duration::from_secs(20);

/// Every test here writes root cells and asserts on them as soon as the write
/// returns. The root tick is shared by the whole process, so a root batch on
/// one test thread can be drained by another test's thread and return before
/// its own propagation is visible. Running the tests one at a time keeps each
/// assertion about root on the thread that drained it.
static ROOT_TICK_SERIAL: Mutex<()> = Mutex::new(());

#[derive(Default)]
struct GateState {
    armed: bool,
    entered: bool,
    open: bool,
}

/// Parks whoever calls [`Gate::pass`] once armed, until [`Gate::open`].
#[derive(Default)]
struct Gate {
    state: Mutex<GateState>,
    changed: Condvar,
}

impl Gate {
    fn arm(&self) {
        self.state.lock().armed = true;
    }

    fn pass(&self) {
        let mut state = self.state.lock();
        if !state.armed {
            return;
        }
        state.entered = true;
        self.changed.notify_all();
        while !state.open {
            if self.changed.wait_for(&mut state, DEADLINE).timed_out() {
                return;
            }
        }
    }

    fn wait_entered(&self) -> bool {
        let mut state = self.state.lock();
        while !state.entered {
            if self.changed.wait_for(&mut state, DEADLINE).timed_out() {
                return false;
            }
        }
        true
    }

    fn is_open(&self) -> bool {
        self.state.lock().open
    }

    fn open(&self) {
        self.state.lock().open = true;
        self.changed.notify_all();
    }
}

/// A source and a derived cell: writing the source and reading the derived
/// cell proves one write and one propagation hop completed.
struct Probe {
    source: Cell<i64, CellMutable>,
    derived: Cell<i64, hyphae::CellImmutable>,
}

impl Probe {
    fn new() -> Self {
        let source = Cell::new(0i64);
        let derived = source.clone().map(|v| v.saturating_mul(2)).materialize();
        Self { source, derived }
    }
}

/// Assert that `domain` and root both complete a batched and an unbatched
/// write-plus-propagation right now, on this thread, while `busy` is still
/// parked inside another domain's wave.
fn assert_progress_while_parked(busy: &Gate, domain: &Domain, probe: &Probe, root: &Probe) {
    assert!(!busy.is_open());

    domain.batch(|| probe.source.set(21));
    assert_eq!(probe.derived.get(), 42);
    probe.source.set(4);
    assert_eq!(probe.derived.get(), 8);

    batch(|| root.source.set(50));
    assert_eq!(root.derived.get(), 100);
    root.source.set(6);
    assert_eq!(root.derived.get(), 12);

    assert!(!busy.is_open());
}

type Log<T> = Arc<Mutex<Vec<T>>>;

fn log_values<T: Clone + Send + 'static>(
    log: &Log<T>,
) -> impl Fn(&Signal<T>) + Send + Sync + use<T> {
    let log = Arc::clone(log);
    move |signal| {
        if let Signal::Value(value) = signal {
            log.lock().push(value.as_ref().clone());
        }
    }
}

// (a) isolation ------------------------------------------------------------

#[test]
fn a_long_wave_in_one_child_does_not_delay_a_sibling_or_root() {
    let _serial = ROOT_TICK_SERIAL.lock();
    let busy = Domain::child();
    let other = Domain::child();
    let gate = Arc::new(Gate::default());

    let (busy_source, busy_derived) = busy.enter(|| {
        let source = Cell::new(0i64);
        let gate = Arc::clone(&gate);
        let derived = source
            .clone()
            .map(move |v| {
                gate.pass();
                *v
            })
            .materialize();
        (source, derived)
    });
    let other_probe = other.enter(Probe::new);
    let root_probe = Probe::new();

    gate.arm();
    let writer = thread::spawn(move || busy.batch(|| busy_source.set(1)));
    assert!(gate.wait_entered(), "the busy child's wave never started");

    assert_progress_while_parked(&gate, &other, &other_probe, &root_probe);

    gate.open();
    assert!(writer.join().is_ok());
    assert_eq!(busy_derived.get(), 1);
}

#[test]
fn a_graph_built_inside_one_childs_wave_does_not_delay_a_sibling_or_root() {
    let _serial = ROOT_TICK_SERIAL.lock();
    let busy = Domain::child();
    let other = Domain::child();
    let gate = Arc::new(Gate::default());
    let born_in: Log<(Domain, Domain)> = Arc::new(Mutex::new(Vec::new()));

    let (selector, switched) = busy.enter(|| {
        let selector = Cell::new(0i64);
        let gate = Arc::clone(&gate);
        let born_in = Arc::clone(&born_in);
        let switched = selector
            .clone()
            .switch_map(move |v| {
                gate.pass();
                let base = *v;
                let leaf = Cell::new(base);
                let inner = leaf.map(|x| x.saturating_add(1)).materialize();
                born_in.lock().push((Domain::current(), inner.domain()));
                inner
            })
            .materialize();
        (selector, switched)
    });
    let other_probe = other.enter(Probe::new);
    let root_probe = Probe::new();

    gate.arm();
    let writer = {
        let busy = busy.clone();
        let selector = selector.clone();
        thread::spawn(move || busy.batch(|| selector.set(1)))
    };
    assert!(
        gate.wait_entered(),
        "the busy child's rebuild never started"
    );

    assert_progress_while_parked(&gate, &other, &other_probe, &root_probe);

    gate.open();
    assert!(writer.join().is_ok());
    assert_eq!(switched.get(), 2);

    // A write from a thread outside any scope still rebuilds inside the child.
    selector.set(5);
    assert_eq!(switched.get(), 6);

    let born_in = born_in.lock().clone();
    assert_eq!(born_in.len(), 3);
    for (ambient, cell_domain) in born_in {
        assert_eq!(ambient, busy);
        assert_eq!(cell_domain, busy);
    }
}

/// Parks its dropper once armed.
#[derive(Debug)]
struct SlowDrop(Arc<Gate>);

impl Drop for SlowDrop {
    fn drop(&mut self) {
        self.0.pass();
    }
}

impl std::fmt::Debug for Gate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Gate")
    }
}

#[test]
fn a_graph_dropped_inside_one_childs_wave_does_not_delay_a_sibling_or_root() {
    let _serial = ROOT_TICK_SERIAL.lock();
    let busy = Domain::child();
    let other = Domain::child();
    let gate = Arc::new(Gate::default());

    let (selector, switched) = busy.enter(|| {
        let selector = Cell::new(0i64);
        let gate = Arc::clone(&gate);
        let switched = selector
            .clone()
            .switch_map(move |v| {
                // Each inner graph owns a value whose drop parks once armed.
                let held = Arc::new(SlowDrop(Arc::clone(&gate)));
                let base = *v;
                Cell::new(base)
                    .map(move |x| {
                        let _held = &held;
                        x.saturating_add(1)
                    })
                    .materialize()
            })
            .materialize();
        (selector, switched)
    });
    let other_probe = other.enter(Probe::new);
    let root_probe = Probe::new();

    gate.arm();
    let writer = thread::spawn(move || busy.batch(|| selector.set(1)));
    assert!(
        gate.wait_entered(),
        "replacing the inner graph never dropped the old one"
    );

    assert_progress_while_parked(&gate, &other, &other_probe, &root_probe);

    gate.open();
    assert!(writer.join().is_ok());
    assert_eq!(switched.get(), 2);
}

// (b) hand-off -------------------------------------------------------------

/// What one child observes of the shared root cells.
struct Reader {
    domain: Domain,
    direct: Log<i64>,
    derived: Log<i64>,
    events: Log<i64>,
    threads: Log<ThreadId>,
    _keep: Vec<SubscriptionGuard>,
}

fn reader(root: &Cell<i64, CellMutable>, events: &Cell<i64, CellMutable>) -> Reader {
    let domain = Domain::child();
    let direct = Log::default();
    let derived = Log::default();
    let event_log = Log::default();
    let threads: Log<ThreadId> = Log::default();
    let keep = domain.enter(|| {
        let seen_on = Arc::clone(&threads);
        let record = log_values(&direct);
        let direct_guard = root.subscribe(move |signal| {
            seen_on.lock().push(thread::current().id());
            record(signal);
        });
        let mapped = root.clone().map(|v| *v).materialize();
        let derived_guard = mapped.subscribe(log_values(&derived));
        let event_guard = events.subscribe(log_values(&event_log));
        vec![direct_guard, derived_guard, event_guard]
    });
    Reader {
        domain,
        direct,
        derived,
        events: event_log,
        threads,
        _keep: keep,
    }
}

#[test]
fn every_reading_child_observes_each_root_write_once_in_root_order() {
    let _serial = ROOT_TICK_SERIAL.lock();
    let root = Cell::new(0i64);
    let events = no_coalesce(|| Cell::new(0i64));

    // The reference: what subscribers in root observe.
    let root_direct = Log::default();
    let root_derived = Log::default();
    let root_events = Log::default();
    let root_mapped = root.clone().map(|v| *v).materialize();
    let _root_guards = [
        root.subscribe(log_values(&root_direct)),
        root_mapped.subscribe(log_values(&root_derived)),
        events.subscribe(log_values(&root_events)),
    ];

    let readers: Vec<Reader> = (0..3).map(|_| reader(&root, &events)).collect();

    // Keep the first child busy in a wave of its own for the first half.
    let gate = Arc::new(Gate::default());
    let busy_writer = readers.first().map(|busy| {
        let domain = busy.domain.clone();
        let gate = Arc::clone(&gate);
        let (source, derived) = domain.enter(|| {
            let source = Cell::new(0i64);
            let derived = source
                .clone()
                .map(move |v| {
                    gate.pass();
                    *v
                })
                .materialize();
            (source, derived)
        });
        thread::spawn(move || {
            domain.batch(|| source.set(1));
            derived.get()
        })
    });
    gate.arm();
    assert!(gate.wait_entered());

    let write_round = |base: i64| {
        for step in 1..=50 {
            root.set(base.saturating_add(step));
        }
        // A root batch coalesces `root` and keeps every `events` value.
        batch(|| {
            root.set(base.saturating_add(1000));
            root.set(base.saturating_add(2000));
            events.set(base.saturating_add(1));
            events.set(base.saturating_add(2));
            events.set(base.saturating_add(3));
        });
    };

    write_round(0);
    assert!(!gate.is_open());
    gate.open();
    write_round(10_000);

    if let Some(writer) = busy_writer {
        assert!(matches!(writer.join(), Ok(1)));
    }

    let writer_thread = thread::current().id();
    let expected_direct = root_direct.lock().clone();
    let expected_derived = root_derived.lock().clone();
    let expected_events = root_events.lock().clone();
    assert_eq!(expected_direct.len(), 103);
    assert_eq!(expected_events.len(), 7);

    for reader in &readers {
        reader.domain.flush();
        assert_eq!(reader.domain.pending_hand_offs(), 0);
        assert_eq!(*reader.direct.lock(), expected_direct);
        assert_eq!(*reader.derived.lock(), expected_derived);
        assert_eq!(*reader.events.lock(), expected_events);
        // Only the subscribe-time replay runs on the subscribing thread; every
        // hand-off delivery runs off the root writer's thread.
        let threads = reader.threads.lock().clone();
        assert_eq!(threads.first(), Some(&writer_thread));
        assert!(threads.iter().skip(1).all(|id| *id != writer_thread));
    }
}

// (c) a slow child ---------------------------------------------------------

#[test]
fn a_slow_child_does_not_delay_root_or_a_sibling_reading_the_same_write() {
    let _serial = ROOT_TICK_SERIAL.lock();
    let root = Cell::new(0i64);
    let root_log = Log::default();
    let _root_guard = root.subscribe(log_values(&root_log));

    let slow = Domain::child();
    let fast = Domain::child();
    let gate = Arc::new(Gate::default());
    let slow_log = Log::default();
    let fast_log = Log::default();

    let _slow_guard = slow.enter(|| {
        let gate = Arc::clone(&gate);
        let record = log_values(&slow_log);
        root.subscribe(move |signal| {
            gate.pass();
            record(signal);
        })
    });
    let _fast_guard = fast.enter(|| root.subscribe(log_values(&fast_log)));

    gate.arm();
    root.set(1);
    assert_eq!(*root_log.lock(), vec![0, 1]);
    assert!(
        gate.wait_entered(),
        "the slow child never received the write"
    );

    fast.flush();
    assert_eq!(*fast_log.lock(), vec![0, 1]);
    assert_eq!(*slow_log.lock(), vec![0]);

    root.set(2);
    batch(|| root.set(3));
    assert_eq!(*root_log.lock(), vec![0, 1, 2, 3]);
    fast.flush();
    assert_eq!(*fast_log.lock(), vec![0, 1, 2, 3]);
    assert!(!gate.is_open());
    assert_eq!(*slow_log.lock(), vec![0]);
    assert_eq!(slow.pending_hand_offs(), 3);

    gate.open();
    slow.flush();
    assert_eq!(*slow_log.lock(), vec![0, 1, 2, 3]);
    assert_eq!(slow.pending_hand_offs(), 0);
}

// (d) direction rule -------------------------------------------------------

fn rejected<R>(build: impl FnOnce() -> R) -> bool {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(build)).is_err()
}

#[test]
fn a_root_cell_cannot_depend_on_a_child_cell() {
    let _serial = ROOT_TICK_SERIAL.lock();
    let child = Domain::child();
    let child_cell = child.enter(|| Cell::new(1i64));

    assert!(rejected(|| child_cell.clone().map(|v| *v).materialize()));

    let root_cell = Cell::new(0i64);
    assert_eq!(
        root_cell.try_own(child_cell.subscribe(|_| {})),
        Err(DomainError::RootReadsChild)
    );
    assert_eq!(root_cell.owned_count(), 0);
    assert_eq!(child_cell.subscriber_count(), 0);
}

#[test]
fn a_cell_cannot_depend_on_a_sibling_child() {
    let _serial = ROOT_TICK_SERIAL.lock();
    let left = Domain::child();
    let right = Domain::child();
    let left_cell = left.enter(|| Cell::new(1i64));
    let right_cell = right.enter(|| Cell::new(2i64));

    assert!(rejected(|| {
        right.enter(|| left_cell.clone().map(|v| *v).materialize())
    }));
    assert!(rejected(|| {
        left.enter(|| left_cell.clone().join(right_cell.clone()).materialize())
    }));
    assert_eq!(
        right.enter(|| right_cell.try_own(left_cell.subscribe(|_| {}))),
        Err(DomainError::SiblingRead)
    );
    assert_eq!(left_cell.subscriber_count(), 0);
}

#[test]
fn a_child_reads_root_only_through_a_subscription_made_in_its_scope() {
    let _serial = ROOT_TICK_SERIAL.lock();
    let child = Domain::child();
    let root_cell = Cell::new(1i64);
    let child_cell = child.enter(|| Cell::new(0i64));

    assert_eq!(
        child_cell.try_own(root_cell.subscribe(|_| {})),
        Err(DomainError::UnscopedBoundary)
    );

    let (derived, local) = child.enter(|| {
        let derived = root_cell.clone().map(|v| v.saturating_add(1)).materialize();
        let local = child_cell
            .clone()
            .map(|v| v.saturating_add(1))
            .materialize();
        (derived, local)
    });
    assert_eq!(derived.domain(), child);
    assert_eq!(local.domain(), child);
    assert_eq!(root_cell.domain(), Domain::root());

    root_cell.set(10);
    child.flush();
    assert_eq!(derived.get(), 11);
}

// (e) glitch-freedom -------------------------------------------------------

#[test]
fn a_child_diamond_over_root_and_local_sources_settles_once_per_batch() {
    let _serial = ROOT_TICK_SERIAL.lock();
    let first = Cell::new(0i64);
    let second = Cell::new(0i64);
    let child = Domain::child();
    let fires = Arc::new(AtomicU64::new(0));
    let seen: Log<(i64, i64, i64, i64)> = Log::default();

    let (local, _guard) = child.enter(|| {
        let local = Cell::new(0i64);
        let plus = first.clone().map(|v| v.saturating_add(1)).materialize();
        let times = first.clone().map(|v| v.saturating_mul(10)).materialize();
        let other = second.clone().map(|v| *v).materialize();
        let fires = Arc::clone(&fires);
        let record = Arc::clone(&seen);
        let sink = plus
            .join(times)
            .materialize()
            .join(other)
            .materialize()
            .join(local.clone())
            .map(|(((plus, times), other), local)| (*plus, *times, *other, *local))
            .materialize();
        let guard = sink.subscribe(move |signal| {
            if let Signal::Value(value) = signal {
                fires.fetch_add(1, Ordering::SeqCst);
                record.lock().push(**value);
            }
        });
        (local, guard)
    });
    assert_eq!(fires.swap(0, Ordering::SeqCst), 1);
    seen.lock().clear();

    // One root batch writing both root sources is one child tick.
    for step in 1..=20i64 {
        batch(|| {
            first.set(step);
            second.set(step.saturating_mul(100));
        });
        child.flush();
        assert_eq!(fires.swap(0, Ordering::SeqCst), 1);
    }

    // A child batch writing the local source is one child tick too.
    child.batch(|| {
        local.set(7);
        local.set(8);
    });
    assert_eq!(fires.swap(0, Ordering::SeqCst), 1);

    // Root batches racing unbatched local writes never expose a torn diamond.
    let racer = thread::spawn(move || {
        for step in 0..200i64 {
            local.set(step);
        }
    });
    for step in 21..=220i64 {
        batch(|| {
            first.set(step);
            second.set(step.saturating_mul(100));
        });
    }
    assert!(racer.join().is_ok());
    child.flush();

    let seen = seen.lock();
    assert!(!seen.is_empty());
    for (plus, times, other, _local) in seen.iter() {
        let base = plus.saturating_sub(1);
        assert_eq!(*times, base.saturating_mul(10), "torn within one root cell");
        assert_eq!(
            *other,
            base.saturating_mul(100),
            "torn across one root batch"
        );
    }
    assert_eq!(
        seen.last().map(|v| (v.0, v.1, v.2)),
        Some((221, 2200, 22_000))
    );
}

// (f) default behaviour ----------------------------------------------------

#[test]
fn a_program_without_child_domains_runs_in_root() {
    let _serial = ROOT_TICK_SERIAL.lock();
    assert!(Domain::current().is_root());
    assert_eq!(Domain::current(), Domain::root());

    let source = Cell::new(1i64);
    let plus = source.clone().map(|v| v.saturating_add(1)).materialize();
    let times = source.clone().map(|v| v.saturating_mul(10)).materialize();
    let fires = Arc::new(AtomicU64::new(0));
    let counter = Arc::clone(&fires);
    let sink = plus.join(times).materialize();
    let _guard = sink.subscribe(move |_| {
        counter.fetch_add(1, Ordering::SeqCst);
    });
    assert_eq!(source.domain(), Domain::root());
    assert_eq!(sink.domain(), Domain::root());
    fires.store(0, Ordering::SeqCst);

    source.set(2);
    assert_eq!(sink.get(), (3, 20));
    assert_eq!(fires.swap(0, Ordering::SeqCst), 2);

    batch(|| source.set(3));
    assert_eq!(sink.get(), (4, 30));
    assert_eq!(fires.swap(0, Ordering::SeqCst), 1);

    // `Domain::root().batch` is `batch`.
    Domain::root().batch(|| source.set(4));
    assert_eq!(sink.get(), (5, 40));
    assert_eq!(fires.load(Ordering::SeqCst), 1);
}
