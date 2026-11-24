//! `Domain::propagating` names the domain whose propagation the calling
//! thread is running, in every place a callback can run.

use std::{
    sync::Arc,
    thread::{self, ThreadId},
    time::Duration,
};

mod domain_support;

use domain_support::{DEADLINE, Log, ROOT_TICK_SERIAL};
use hyphae::{
    Cell, CellMap, Gettable, Mutable, Source, Watchable, batch, interval,
    scheduler::{Domain, set_wave_threshold_for_test},
};

type Seen = Log<(Option<Domain>, ThreadId)>;

/// A callback that records what `Domain::propagating` returns where it runs.
fn record<T>(seen: &Seen) -> impl Fn(&T) + Send + Sync + use<T> {
    let seen = Arc::clone(seen);
    move |_| {
        seen.lock()
            .push((Domain::propagating(), thread::current().id()));
    }
}

/// The domains recorded after the first `skip` calls.
fn domains(seen: &Seen, skip: usize) -> Vec<Option<Domain>> {
    seen.lock()
        .iter()
        .skip(skip)
        .map(|(domain, _)| domain.clone())
        .collect()
}

fn wait_for(what: &str, done: impl Fn() -> bool) {
    let started = std::time::Instant::now();
    while !done() {
        assert!(started.elapsed() < DEADLINE, "{what}");
        thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn a_thread_running_no_propagation_is_in_none() {
    let _serial = ROOT_TICK_SERIAL.lock();
    let child = Domain::child();

    assert_eq!(Domain::propagating(), None, "an idle thread");
    assert_eq!(child.enter(Domain::propagating), None, "a child's scope");
    assert_eq!(
        Domain::root().enter(Domain::propagating),
        None,
        "root's scope"
    );
    assert_eq!(child.batch(Domain::propagating), None, "a child's batch");
    assert_eq!(batch(Domain::propagating), None, "a root batch");
    assert_eq!(child.enter(Domain::current), child, "the scope is ambient");

    // The call a new subscription gets with the current value.
    let seen = Seen::default();
    let cell = Cell::new(0i64);
    let _guard = cell.subscribe(record(&seen));
    let _child_guard = child.enter(|| cell.subscribe(record(&seen)));
    assert_eq!(domains(&seen, 0), [None, None]);
}

#[test]
fn an_eager_cascade_is_in_the_written_domain() {
    let _serial = ROOT_TICK_SERIAL.lock();
    let child = Domain::child();
    for domain in [Domain::root(), child.clone()] {
        let seen = Seen::default();
        let (cell, map, source) = domain.enter(|| {
            (
                Cell::new(0i64),
                CellMap::<u32, i64>::new(),
                Source::<i64>::new(),
            )
        });
        let _guards = domain.enter(|| {
            (
                cell.subscribe(record(&seen)),
                map.subscribe_diffs(record(&seen)),
                source.subscribe(record(&seen)),
            )
        });
        let subscribed = seen.lock().len();

        cell.set(1);
        map.insert(1, 1);
        source.emit(1);

        let here = thread::current().id();
        assert_eq!(
            domains(&seen, subscribed),
            [
                Some(domain.clone()),
                Some(domain.clone()),
                Some(domain.clone())
            ],
            "{domain:?}"
        );
        assert!(seen.lock().iter().all(|(_, thread)| *thread == here));
        assert_eq!(Domain::propagating(), None, "after the cascade");
    }
    assert!(!child.is_closed(), "a returned handle closed the domain");
}

#[test]
fn a_wave_is_in_the_drained_domain() {
    let _serial = ROOT_TICK_SERIAL.lock();
    for domain in [Domain::root(), Domain::child()] {
        let seen = Seen::default();
        let cell = domain.enter(|| Cell::new(0i64));
        let _guard = domain.enter(|| cell.subscribe(record(&seen)));

        domain.batch(|| {
            cell.set(1);
            assert_eq!(
                Domain::propagating(),
                None,
                "inside the batch, before it drains"
            );
        });

        assert_eq!(domains(&seen, 1), [Some(domain.clone())], "{domain:?}");
    }
}

#[test]
fn a_root_wave_on_the_worker_pool_is_in_root() {
    let _serial = ROOT_TICK_SERIAL.lock();
    let _child = Domain::child();
    let seen = Seen::default();
    let cells: Vec<_> = (0..64).map(|_| Cell::new(0i64)).collect();
    let _guards: Vec<_> = cells
        .iter()
        .map(|cell| cell.subscribe(record(&seen)))
        .collect();
    let subscribed = seen.lock().len();

    set_wave_threshold_for_test(2);
    batch(|| cells.iter().for_each(|cell| cell.set(1)));
    set_wave_threshold_for_test(256);

    let delivered = domains(&seen, subscribed);
    assert_eq!(delivered.len(), cells.len());
    assert!(
        delivered
            .iter()
            .all(|domain| *domain == Some(Domain::root()))
    );
    let here = thread::current().id();
    assert!(
        seen.lock()
            .iter()
            .skip(subscribed)
            .any(|(_, thread)| *thread != here),
        "the wave never left the batching thread, so the pool was not exercised"
    );
}

#[test]
fn a_hand_off_unit_is_in_the_child() {
    let _serial = ROOT_TICK_SERIAL.lock();
    let child = Domain::child();
    let seen = Seen::default();
    let shared = Cell::new(0i64);
    let _guard = child.enter(|| shared.subscribe(record(&seen)));
    let here = thread::current().id();

    // An eager unit, a unit made by a root batch, and an explicit hand-off.
    shared.set(1);
    batch(|| shared.set(2));
    child.hand_off(record::<()>(&seen).pipe(|record| move || record(&())));
    child.flush();

    let delivered = seen.lock().iter().skip(1).cloned().collect::<Vec<_>>();
    assert_eq!(delivered.len(), 3);
    for (domain, thread) in delivered {
        assert_eq!(domain, Some(child.clone()));
        assert_ne!(thread, here, "the unit ran on the root writer's thread");
    }
}

#[test]
fn a_childs_timer_callback_is_in_the_child() {
    let _serial = ROOT_TICK_SERIAL.lock();
    let child = Domain::child();
    let seen = Seen::default();
    let (_ticks, _guard) = child.enter(|| {
        let ticks = interval(Duration::from_millis(2));
        let guard = ticks.subscribe(record(&seen));
        (ticks, guard)
    });

    wait_for("the child's timer never fired", || seen.lock().len() > 1);
    assert!(
        domains(&seen, 1)
            .iter()
            .all(|domain| *domain == Some(child.clone()))
    );
}

#[test]
fn a_scope_entered_inside_a_propagation_does_not_change_it() {
    let _serial = ROOT_TICK_SERIAL.lock();
    let child = Domain::child();
    // Root's subscriber enters the child's scope, the child's its own.
    for (written, entered) in [(Domain::root(), child.clone()), (child.clone(), child)] {
        let inside: Log<(Option<Domain>, Domain)> = Log::default();
        let cell = written.enter(|| Cell::new(0i64));
        let _guard = written.enter(|| {
            let (entered, inside) = (entered.clone(), Arc::clone(&inside));
            cell.subscribe(move |_| {
                let seen = entered.enter(|| (Domain::propagating(), Domain::current()));
                inside.lock().push(seen);
            })
        });

        cell.set(1);

        assert_eq!(
            inside.lock().last().cloned(),
            Some((Some(written.clone()), entered.clone())),
            "written {written:?}, entered {entered:?}"
        );
    }
}

#[test]
fn a_root_subscriber_uses_it_to_hand_a_child_write_over() {
    let _serial = ROOT_TICK_SERIAL.lock();
    let child = Domain::child();
    let target = child.enter(|| Cell::new(0i64));
    let shared = Cell::new(0i64);

    // The same code runs from root's propagation and from an idle thread.
    let write = {
        let (child, target) = (child.clone(), target.clone());
        move |value: i64| match Domain::propagating() {
            Some(domain) if domain != child => {
                let target = target.clone();
                child.hand_off(move || target.set(value));
            }
            _ => target.set(value),
        }
    };
    let _guard = {
        let write = write.clone();
        shared.subscribe(move |signal| {
            if let hyphae::Signal::Value(value) = signal {
                write(**value);
            }
        })
    };

    shared.set(7);
    child.flush();
    assert_eq!(target.get(), 7, "from a root cascade");

    write(8);
    assert_eq!(target.get(), 8, "from an idle thread, with no hand-off");
}

trait Pipe: Sized {
    fn pipe<R>(self, f: impl FnOnce(Self) -> R) -> R {
        f(self)
    }
}

impl<T> Pipe for T {}

#[test]
fn entering_root_or_a_sibling_from_a_childs_propagation_panics() {
    let _serial = ROOT_TICK_SERIAL.lock();
    let child = Domain::child();
    let sibling = Domain::child();
    for entered in [Domain::root(), sibling] {
        let cell = child.enter(|| Cell::new(0i64));
        let refused: Log<String> = Log::default();
        let _guard = child.enter(|| {
            let (entered, refused) = (entered.clone(), Arc::clone(&refused));
            cell.subscribe(move |value| {
                if !matches!(value, hyphae::Signal::Value(value) if **value != 0) {
                    return;
                }
                let entered = entered.clone();
                let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                    entered.enter(|| ())
                }));
                let message = panicked
                    .err()
                    .and_then(|payload| payload.downcast_ref::<String>().cloned())
                    .unwrap_or_default();
                refused.lock().push(message);
            })
        });

        cell.set(1);

        let refused = refused.lock().last().cloned().unwrap_or_default();
        assert!(
            refused.contains(&hyphae::scheduler::DomainError::EnterAgainstFlow.to_string()),
            "entering {entered:?} from the child's wave: {refused:?}"
        );
    }
}
