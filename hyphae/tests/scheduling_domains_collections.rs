//! Maps, map queries, joins and dynamic operators across the domain boundary.
//!
//! The shape under test: a map in root holds shared data, and queries and
//! joins that live in child domains read it. Every child gets every root map
//! diff exactly once, in root order, on its own tick. A map born in a child is
//! written from any thread and never touches root.

#![allow(clippy::arithmetic_side_effects)]

use std::{
    sync::Arc,
    thread::{self, ThreadId},
};

mod domain_support;

use domain_support::{Gate, Log, ROOT_TICK_SERIAL};
use hyphae::{
    Cell, CellImmutable, CellMap, CountByExt, DepNode, Gettable, GroupByExt, InnerJoinExt,
    LeftJoinExt, MapDiff, MapEntriesExt, MapExt, MapQuery, Materialize, Mutable, SelectExt, Signal,
    SwitchMapExt, Watchable, batch, cell_map::MapWrite, scheduler::Domain,
};

type Row = (u32, i64);
type Diffs = Log<MapDiff<u32, i64>>;

fn panics<R>(run: impl FnOnce() -> R) -> bool {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(run)).is_err()
}

fn sorted<K: Ord + Clone, V: Clone>(mut entries: Vec<(K, V)>) -> Vec<(K, V)> {
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    entries
}

/// Park `domain`'s worker inside a unit until the gate opens.
fn park(domain: &Domain) -> Arc<Gate> {
    let gate = Arc::new(Gate::default());
    gate.arm();
    domain.hand_off({
        let gate = Arc::clone(&gate);
        move || gate.pass()
    });
    assert!(gate.wait_entered(), "the worker never took the unit");
    gate
}

// the diff stream ------------------------------------------------------------

#[test]
fn every_child_receives_each_root_map_diff_once_in_root_order() {
    let _serial = ROOT_TICK_SERIAL.lock();
    let shared = CellMap::<u32, i64>::new();
    shared.insert(1, 10);

    let reference: Diffs = Log::default();
    let _reference_guard = {
        let reference = Arc::clone(&reference);
        shared.subscribe_diffs(move |diff| reference.lock().push(diff.clone()))
    };

    let readers: Vec<_> = (0..3)
        .map(|_| {
            let child = Domain::child();
            let diffs: Diffs = Log::default();
            let threads: Log<ThreadId> = Log::default();
            let guard = child.enter(|| {
                let diffs = Arc::clone(&diffs);
                let threads = Arc::clone(&threads);
                shared.subscribe_diffs(move |diff| {
                    threads.lock().push(thread::current().id());
                    diffs.lock().push(diff.clone());
                })
            });
            (child, diffs, threads, guard)
        })
        .collect();

    let write_round = |base: u32| {
        shared.insert(base + 2, 20);
        shared.insert(base + 2, 21);
        shared.remove(&(base + 2));
        shared.insert_many(vec![(base + 3, 30), (base + 4, 40)]);
        shared.remove_many(vec![base + 3]);
        batch(|| {
            shared.insert(base + 5, 50);
            shared.insert(base + 5, 51);
            shared.remove(&(base + 5));
            shared.insert(base + 6, 60);
        });
    };

    // The first child's worker is busy for the first round.
    let gate = readers.first().map(|(child, ..)| park(child));
    write_round(100);
    if let Some(gate) = gate {
        gate.open();
    }
    write_round(200);

    let writer = thread::current().id();
    let expected = reference.lock().clone();
    assert_eq!(expected.len(), 19);
    for (child, diffs, threads, _guard) in &readers {
        child.flush();
        assert_eq!(child.pending_hand_offs(), 0);
        assert_eq!(*diffs.lock(), expected);
        // Only the initial snapshot is delivered on the subscribing thread.
        let threads = threads.lock().clone();
        assert_eq!(threads.first(), Some(&writer));
        assert!(threads.iter().skip(1).all(|id| *id != writer));
    }
}

// per-key observers and length ------------------------------------------------

#[test]
fn per_key_observers_and_length_of_a_root_map_serve_every_child() {
    let _serial = ROOT_TICK_SERIAL.lock();
    let shared = CellMap::<u32, i64>::new();
    let left = Domain::child();
    let right = Domain::child();
    let computed_on: Log<ThreadId> = Log::default();

    let watch = |domain: &Domain| {
        domain.enter(|| {
            let point = shared.get(&7).materialize();
            let computed_on = Arc::clone(&computed_on);
            let doubled = point
                .clone()
                .map(move |value| {
                    computed_on.lock().push(thread::current().id());
                    value.map(|value| value * 2)
                })
                .materialize();
            let length = shared.len().materialize();
            (point, doubled, length)
        })
    };
    let (left_point, left_doubled, left_length) = watch(&left);
    let (right_point, right_doubled, right_length) = watch(&right);

    // The observer of a root map's key is a root cell, whoever asks first.
    assert_eq!(left_point.domain(), Domain::root());
    assert_eq!(right_point.domain(), Domain::root());
    assert_eq!(left_doubled.domain(), left);
    assert_eq!(right_doubled.domain(), right);
    assert_eq!(left_length.domain(), left);
    computed_on.lock().clear();

    let gate = park(&left);
    shared.insert(7, 5);
    shared.insert(8, 1);
    assert_eq!(left_point.get(), Some(5));
    right.flush();
    assert_eq!(right_doubled.get(), Some(10));
    assert_eq!(right_length.get(), 2);
    assert_eq!(left_doubled.get(), None);
    assert_eq!(left_length.get(), 0);

    gate.open();
    left.flush();
    assert_eq!(left_doubled.get(), Some(10));
    assert_eq!(left_length.get(), 2);

    shared.remove(&7);
    left.flush();
    right.flush();
    assert_eq!(left_doubled.get(), None);
    assert_eq!(right_doubled.get(), None);
    assert_eq!(right_length.get(), 1);

    // No child computation ran on the thread that wrote the root map.
    let writer = thread::current().id();
    assert!(computed_on.lock().iter().all(|id| *id != writer));
}

// queries and joins -----------------------------------------------------------

/// The queries one consumer builds over the shared map and a map of its own.
struct Views {
    selected: CellMap<u32, Row, CellImmutable>,
    renamed: CellMap<u64, i64, CellImmutable>,
    by_team: CellMap<u32, Vec<Row>, CellImmutable>,
    team_sizes: CellMap<u32, usize, CellImmutable>,
    with_bonus: CellMap<u32, (Row, i64), CellImmutable>,
    with_notes: CellMap<u32, (Row, Vec<i64>), CellImmutable>,
    bonus: CellMap<u32, i64>,
    notes: CellMap<u32, i64>,
}

impl Views {
    fn over(shared: &CellMap<u32, Row>, teams: &CellMap<u32, i64>) -> Self {
        let bonus = CellMap::<u32, i64>::new();
        let notes = CellMap::<u32, i64>::new();
        Self {
            selected: shared.clone().select(|row| row.1 >= 10).materialize(),
            renamed: shared
                .clone()
                .filter_map_entries(|key, row| (row.1 % 2 == 0).then(|| (u64::from(*key), row.1)))
                .materialize(),
            by_team: shared.clone().group_by(|_, row| row.0).materialize(),
            team_sizes: shared.clone().count_by(|_, row| row.0).materialize(),
            with_bonus: shared.clone().inner_join(bonus.clone()).materialize(),
            // A join region over two root maps and one of this consumer's own.
            with_notes: shared
                .clone()
                .left_join_by(teams.clone(), |_, row| row.0, |team, _| *team)
                .map_joined_values(|_, row, teams| {
                    (
                        row.0,
                        row.1 + teams.iter().map(|(_, bias)| *bias).sum::<i64>(),
                    )
                })
                .left_join_by(notes.clone(), |key, _| *key, |key, _| *key)
                .materialize(),
            bonus,
            notes,
        }
    }

    fn write_own(&self, step: i64) {
        self.bonus.insert(1, step);
        self.bonus.insert(2, step + 1);
        self.notes.insert_many(vec![(1, step), (3, step)]);
        self.bonus.remove(&2);
    }

    #[allow(clippy::type_complexity)]
    fn state(
        &self,
    ) -> (
        Vec<(u32, Row)>,
        Vec<(u64, i64)>,
        Vec<(u32, Vec<Row>)>,
        Vec<(u32, usize)>,
        Vec<(u32, (Row, i64))>,
        Vec<(u32, (Row, Vec<i64>))>,
    ) {
        let by_team = sorted(self.by_team.snapshot())
            .into_iter()
            .map(|(team, mut rows)| {
                rows.sort_unstable();
                (team, rows)
            })
            .collect();
        (
            sorted(self.selected.snapshot()),
            sorted(self.renamed.snapshot()),
            by_team,
            sorted(self.team_sizes.snapshot()),
            sorted(self.with_bonus.snapshot()),
            sorted(self.with_notes.snapshot()),
        )
    }
}

#[test]
fn queries_and_joins_in_children_follow_root_maps() {
    let _serial = ROOT_TICK_SERIAL.lock();
    let shared = CellMap::<u32, Row>::new();
    let teams = CellMap::<u32, i64>::new();
    shared.insert(1, (1, 4));
    teams.insert(1, 100);

    let reference = Views::over(&shared, &teams);
    let children: Vec<(Domain, Views)> = (0..2)
        .map(|_| {
            let child = Domain::child();
            let views = child.enter(|| Views::over(&shared, &teams));
            (child, views)
        })
        .collect();
    for (child, views) in &children {
        assert_eq!(views.selected.domain(), *child);
        assert_eq!(views.with_notes.domain(), *child);
        assert_eq!(views.bonus.domain(), *child);
    }
    assert_eq!(shared.domain(), Domain::root());

    let gate = children.first().map(|(child, _)| park(child));
    for step in 0..20u32 {
        let score = i64::from(step);
        shared.insert(step % 5 + 1, (step % 3, score));
        batch(|| {
            shared.insert(40, (9, 10 + score));
            shared.insert(41, (9, 20 + score));
            teams.insert(step % 3, score);
        });
        if step % 4 == 0 {
            shared.remove(&(step % 5 + 1));
        }
        batch(|| {
            shared.remove(&40);
            shared.remove(&41);
        });
        reference.write_own(score);
        for (_, views) in &children {
            views.write_own(score);
        }
    }
    if let Some(gate) = gate {
        gate.open();
    }

    let expected = reference.state();
    assert!(!expected.5.is_empty());
    for (child, views) in &children {
        child.flush();
        assert_eq!(views.state(), expected);
    }
}

#[test]
fn a_root_batch_of_map_writes_is_one_step_of_a_childs_query() {
    let _serial = ROOT_TICK_SERIAL.lock();
    let shared = CellMap::<u32, i64>::new();
    let child = Domain::child();
    let (selected, entries) = child.enter(|| {
        let selected = shared.clone().select(|value| *value > 0).materialize();
        let entries = selected.entries().materialize();
        (selected, entries)
    });
    let lengths: Log<usize> = Log::default();
    let _guard = child.enter(|| {
        let lengths = Arc::clone(&lengths);
        entries.subscribe(move |signal| {
            if let Signal::Value(entries) = signal {
                lengths.lock().push(entries.len());
            }
        })
    });

    for step in 0..50u32 {
        batch(|| {
            shared.insert(step * 2, 1);
            shared.insert(step * 2 + 1, 1);
        });
    }
    child.flush();
    assert_eq!(selected.snapshot().len(), 100);
    assert_eq!(
        *lengths.lock(),
        (0..=50).map(|step| step * 2).collect::<Vec<usize>>()
    );
}

// child-local maps ------------------------------------------------------------

#[test]
fn a_childs_own_maps_are_written_while_root_is_busy() {
    let _serial = ROOT_TICK_SERIAL.lock();
    let shared = CellMap::<u32, i64>::new();
    let gate = Arc::new(Gate::default());
    let _slow = {
        let gate = Arc::clone(&gate);
        shared.subscribe_diffs(move |_| gate.pass())
    };
    let child = Domain::child();
    let (local, totals, joined) = child.enter(|| {
        let local = CellMap::<u32, i64>::new();
        let totals = local.clone().count_by(|_, value| *value % 2).materialize();
        let joined = local.clone().inner_join(shared.clone()).materialize();
        (local, totals, joined)
    });

    gate.arm();
    let root_writer = thread::spawn(move || shared.insert(1, 1));
    assert!(gate.wait_entered(), "the root cascade never started");

    // Root is inside a cascade. The child's maps, queries and join still move.
    for step in 0..100u32 {
        local.insert(step, i64::from(step));
    }
    local.insert_many(vec![(200, 2), (201, 3)]);
    local.remove(&0);
    child.batch(|| {
        local.insert(300, 4);
        local.insert(301, 5);
    });
    assert_eq!(
        sorted(totals.snapshot()),
        vec![(0, 51), (1, 52)],
        "the child's query did not follow its own map"
    );
    assert!(!gate.is_open());

    gate.open();
    assert!(root_writer.join().is_ok());
    child.flush();
    assert_eq!(joined.snapshot(), vec![(1, (1, 1))]);
}

// dynamic operators -----------------------------------------------------------

#[test]
fn a_child_switch_map_subscribes_to_root_from_inside_its_wave() {
    let _serial = ROOT_TICK_SERIAL.lock();
    let shared = CellMap::<u32, i64>::new();
    shared.insert(1, 10);
    shared.insert(2, 20);
    let child = Domain::child();
    let inner_domains: Log<Domain> = Log::default();

    let (selector, switched) = child.enter(|| {
        let selector = Cell::new(1u32);
        let shared = shared.clone();
        let inner_domains = Arc::clone(&inner_domains);
        let switched = selector
            .clone()
            .switch_map(move |key| {
                let inner = shared
                    .get(key)
                    .map(|value| value.unwrap_or(-1))
                    .materialize();
                inner_domains.lock().push(inner.domain());
                inner
            })
            .materialize();
        (selector, switched)
    });
    let first_key = shared.get(&1).materialize();
    let second_key = shared.get(&2).materialize();
    assert_eq!(switched.get(), 10);
    assert_eq!(first_key.subscriber_count(), 1);

    shared.insert(1, 11);
    child.flush();
    assert_eq!(switched.get(), 11);

    // The switch runs inside the child's wave and subscribes to a root cell.
    child.batch(|| selector.set(2));
    assert_eq!(switched.get(), 20);
    assert_eq!(first_key.subscriber_count(), 0);
    assert_eq!(second_key.subscriber_count(), 1);

    shared.insert(1, 12);
    shared.insert(2, 21);
    child.flush();
    assert_eq!(switched.get(), 21);
    assert!(inner_domains.lock().iter().all(|domain| *domain == child));
    assert_eq!(first_key.domain(), Domain::root());
}

// writes and hooks ------------------------------------------------------------

#[test]
fn a_propagation_cannot_write_another_domains_map() {
    let _serial = ROOT_TICK_SERIAL.lock();
    let shared = CellMap::<u32, i64>::new();
    let child = Domain::child();
    let local = child.enter(CellMap::<u32, i64>::new);

    // A child subscriber that writes the root map.
    let (trigger, _guard) = child.enter(|| {
        let trigger = Cell::new(0i64);
        let shared = shared.clone();
        let guard = trigger.subscribe(move |signal| {
            if let Signal::Value(value) = signal
                && **value > 0
            {
                shared.insert(1, **value);
            }
        });
        (trigger, guard)
    });
    assert!(panics(|| trigger.set(1)));
    assert!(panics(|| child.batch(|| trigger.set(2))));
    assert!(shared.snapshot().is_empty(), "the root map was written");

    // A root subscriber that writes the child's map.
    let root_trigger = Cell::new(0i64);
    let _root_guard = {
        let local = local.clone();
        root_trigger.subscribe(move |signal| {
            if let Signal::Value(value) = signal
                && **value > 0
            {
                local.insert(1, **value);
            }
        })
    };
    assert!(panics(|| root_trigger.set(1)));
    assert!(local.snapshot().is_empty(), "the child map was written");

    // Outside every propagation both maps take writes from any thread.
    shared.insert(1, 1);
    local.insert(1, 1);
    assert_eq!(shared.get_value(&1), Some(1));
    assert_eq!(local.get_value(&1), Some(1));
}

#[test]
fn a_write_hook_runs_with_the_write_whichever_domain_registered_it() {
    let _serial = ROOT_TICK_SERIAL.lock();
    let shared = CellMap::<u32, i64>::new();
    shared.insert(1, 1);
    let child = Domain::child();
    let writes: Log<(ThreadId, u32)> = Log::default();
    let _hook = child.enter(|| {
        let writes = Arc::clone(&writes);
        shared.register_write_hook(move |write| {
            let key = match write {
                MapWrite::Insert { key, .. }
                | MapWrite::Update { key, .. }
                | MapWrite::Remove { key, .. } => *key,
            };
            writes.lock().push((thread::current().id(), key));
        })
    });

    // The hook is part of the write, not of propagation: it has run when the
    // write returns, on the writing thread, even while the child is parked.
    let gate = park(&child);
    shared.insert(2, 2);
    batch(|| shared.insert(3, 3));
    let caller = thread::current().id();
    assert_eq!(*writes.lock(), vec![(caller, 1), (caller, 2), (caller, 3)]);
    gate.open();
}
