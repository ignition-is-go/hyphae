//! A map's diffs reach its subscribers in the order its rows were stored,
//! whichever threads write and whichever of them hold a batch open.
//!
//! After every writer has returned, everything derived from the map equals
//! what the same derivation gives when computed from the map's rows: a
//! materialized `select`, `group_by` and join, the per-key observers, the
//! length, the entries, and a replay of the diff stream. The replay also
//! checks each diff against the row the previous diff left, so a diff
//! delivered out of stored order is caught even when a later one repairs the
//! final state.

#![allow(clippy::arithmetic_side_effects)]

use std::{
    collections::BTreeMap,
    sync::{
        Arc, Barrier,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    thread,
};

mod domain_support;

use domain_support::{Gate, Log, ROOT_TICK_SERIAL, log_values};
use hyphae::{
    Cell, CellImmutable, CellMap, CellSet, Gettable, GroupByExt, InnerJoinExt, JoinExt, MapDiff,
    MapQuery, Materialize, Mutable, SelectExt, SubscriptionGuard, Watchable, cell_map::MapWrite,
    cell_set::SetDiff, scheduler::Domain,
};
use parking_lot::Mutex;

type Rows = BTreeMap<u32, i64>;

fn in_root_and_in_a_child(check: impl Fn(&Domain)) {
    let _serial = ROOT_TICK_SERIAL.lock();
    check(&Domain::root());
    check(&Domain::child());
}

/// A batch held open on another thread, which runs what it is handed.
struct HeldBatch {
    work: Option<mpsc::Sender<Box<dyn FnOnce() + Send>>>,
    done: mpsc::Receiver<()>,
    thread: Option<thread::JoinHandle<()>>,
}

impl HeldBatch {
    fn open(domain: &Domain) -> Self {
        let (work, queue) = mpsc::channel::<Box<dyn FnOnce() + Send>>();
        let (finished, done) = mpsc::channel();
        let domain = domain.clone();
        let thread = thread::spawn(move || {
            domain.batch(|| {
                for step in queue {
                    step();
                    let _ = finished.send(());
                }
            });
        });
        Self {
            work: Some(work),
            done,
            thread: Some(thread),
        }
    }

    /// Run `step` inside the batch and return once it has run.
    fn run(&self, step: impl FnOnce() + Send + 'static) {
        let sent = self
            .work
            .as_ref()
            .is_some_and(|work| work.send(Box::new(step)).is_ok());
        assert!(sent, "the held batch is gone");
        assert!(
            self.done.recv_timeout(domain_support::DEADLINE).is_ok(),
            "a step of the held batch did not finish"
        );
    }

    /// Close the batch and return once its thread has.
    fn close(mut self) {
        self.work = None;
        let closed = self
            .thread
            .take()
            .is_some_and(|thread| thread.join().is_ok());
        assert!(closed, "the held batch panicked");
    }
}

/// Replays a map's diff stream and counts every diff that does not follow
/// from the row the stream left before it.
#[derive(Default)]
struct Replay {
    rows: Mutex<Rows>,
    breaks: Mutex<Vec<String>>,
}

impl Replay {
    fn apply(&self, rows: &mut Rows, diff: &MapDiff<u32, i64>) {
        let had = |rows: &Rows, key: &u32| rows.get(key).copied();
        match diff {
            MapDiff::Initial { entries } => {
                rows.clear();
                rows.extend(entries.iter().copied());
            }
            MapDiff::Insert { key, value } => {
                if let Some(old) = rows.insert(*key, *value) {
                    self.broke(format!("insert of {key} over {old}"));
                }
            }
            MapDiff::Update {
                key,
                old_value,
                new_value,
            } => {
                if had(rows, key) != Some(*old_value) {
                    self.broke(format!(
                        "update of {key} from {old_value} over {:?}",
                        had(rows, key)
                    ));
                }
                rows.insert(*key, *new_value);
            }
            MapDiff::Remove { key, old_value } => {
                if rows.remove(key) != Some(*old_value) {
                    self.broke(format!("remove of {key} holding {old_value}"));
                }
            }
            MapDiff::Batch { changes } => {
                for change in changes {
                    self.apply(rows, change);
                }
            }
        }
    }

    fn broke(&self, what: String) {
        self.breaks.lock().push(what);
    }

    fn watch(self: &Arc<Self>, source: &CellMap<u32, i64>) -> SubscriptionGuard {
        let replay = Arc::clone(self);
        source.subscribe_diffs(move |diff| {
            let mut rows = replay.rows.lock();
            replay.apply(&mut rows, diff);
        })
    }
}

const KEYS: u32 = 6;

const fn group_of(value: i64) -> i64 {
    value.rem_euclid(3)
}

/// Everything the tests derive from one source map.
struct Derived {
    selected: CellMap<u32, i64, CellImmutable>,
    grouped: CellMap<i64, Vec<i64>, CellImmutable>,
    joined: CellMap<u32, (i64, i64), CellImmutable>,
    right: CellMap<u32, i64>,
    points: Vec<Cell<Option<i64>, CellImmutable>>,
    size: Cell<usize, CellImmutable>,
    entries: Cell<Vec<(u32, i64)>, CellImmutable>,
    replay: Arc<Replay>,
    _guard: SubscriptionGuard,
}

impl Derived {
    /// Build inside `domain`, which is the source's own or a child reading a
    /// root source.
    fn of(domain: &Domain, source: &CellMap<u32, i64>) -> Self {
        domain.enter(|| {
            let right = CellMap::<u32, i64>::new();
            for key in 0..KEYS {
                right.insert(key, i64::from(key) * 100);
            }
            let replay = Arc::new(Replay::default());
            Self {
                selected: source.clone().select(|_| true).materialize(),
                grouped: source
                    .clone()
                    .group_by(|_key, value| group_of(*value))
                    .materialize(),
                joined: source.clone().inner_join(right.clone()).materialize(),
                points: (0..KEYS)
                    .map(|key| source.get(&key).materialize())
                    .collect(),
                size: source.size().materialize(),
                entries: source.entries().materialize(),
                _guard: replay.watch(source),
                replay,
                right,
            }
        })
    }

    /// Assert every derived structure equals its recomputation from `rows`.
    fn assert_matches(&self, rows: &Rows, context: &str) {
        let as_rows = |entries: Vec<(u32, i64)>| entries.into_iter().collect::<Rows>();
        assert_eq!(
            *self.replay.breaks.lock(),
            Vec::<String>::new(),
            "{context}: a diff did not follow from the one before it"
        );
        assert_eq!(*self.replay.rows.lock(), *rows, "{context}: diff replay");
        assert_eq!(
            as_rows(self.selected.snapshot()),
            *rows,
            "{context}: select"
        );
        assert_eq!(as_rows(self.entries.get()), *rows, "{context}: entries");
        assert_eq!(self.size.get(), rows.len(), "{context}: length");
        for (key, point) in (0..KEYS).zip(&self.points) {
            assert_eq!(
                point.get(),
                rows.get(&key).copied(),
                "{context}: observer of {key}"
            );
        }

        let mut groups = BTreeMap::<i64, Vec<i64>>::new();
        for value in rows.values() {
            groups.entry(group_of(*value)).or_default().push(*value);
        }
        let grouped: BTreeMap<i64, Vec<i64>> = self
            .grouped
            .snapshot()
            .into_iter()
            .filter(|(_, values)| !values.is_empty())
            .map(|(group, mut values)| {
                values.sort_unstable();
                (group, values)
            })
            .collect();
        for values in groups.values_mut() {
            values.sort_unstable();
        }
        assert_eq!(grouped, groups, "{context}: group_by");

        let right = as_rows(self.right.snapshot());
        let joined: BTreeMap<u32, (i64, i64)> = rows
            .iter()
            .filter_map(|(key, value)| Some((*key, (*value, *right.get(key)?))))
            .collect();
        assert_eq!(
            self.joined
                .snapshot()
                .into_iter()
                .collect::<BTreeMap<_, _>>(),
            joined,
            "{context}: join"
        );
    }
}

fn rows_of(source: &CellMap<u32, i64>) -> Rows {
    source.snapshot().into_iter().collect()
}

/// One thread writes a key inside a batch it keeps open. Another thread then
/// writes the same key. The map holds the second write, so a map derived from
/// it must hold the second write too once both have propagated.
fn assert_a_derived_map_follows_the_later_write(domain: &Domain) {
    let (source, derived) = domain.enter(|| {
        let source = CellMap::<u32, i64>::new();
        let derived = source.clone().select(|_| true).materialize();
        (source, derived)
    });
    let gate = Arc::new(Gate::default());
    gate.arm();

    let earlier = {
        let (domain, source, gate) = (domain.clone(), source.clone(), Arc::clone(&gate));
        thread::spawn(move || {
            domain.batch(|| {
                source.insert(1, 10);
                gate.pass();
            });
        })
    };
    assert!(
        gate.wait_entered(),
        "the earlier writer never opened its batch"
    );

    domain.batch(|| {
        source.insert(1, 20);
    });

    gate.open();
    assert!(earlier.join().is_ok());
    domain.flush();
    assert_eq!(source.get_value(&1), Some(20));
    assert_eq!(
        derived.get_value(&1),
        source.get_value(&1),
        "the derived map holds the earlier write and the source holds the later one"
    );
}

#[test]
fn a_root_map_write_staged_in_an_open_batch_does_not_arrive_after_a_later_write() {
    let _serial = ROOT_TICK_SERIAL.lock();
    assert_a_derived_map_follows_the_later_write(&Domain::root());
}

#[test]
fn a_child_map_write_staged_in_an_open_batch_does_not_arrive_after_a_later_write() {
    let _serial = ROOT_TICK_SERIAL.lock();
    assert_a_derived_map_follows_the_later_write(&Domain::child());
}

#[test]
fn an_unbatched_write_to_a_key_an_open_batch_wrote_is_delivered_after_it() {
    in_root_and_in_a_child(|domain| {
        let source = domain.enter(CellMap::<u32, i64>::new);
        let derived = Derived::of(domain, &source);
        let held = HeldBatch::open(domain);
        held.run({
            let source = source.clone();
            move || {
                source.insert(1, 10);
            }
        });

        source.insert(1, 20);
        assert_eq!(source.remove(&1), Some(20));
        source.insert(1, 30);
        assert_eq!(
            derived.selected.get_value(&1),
            None,
            "a write that follows an open batch's row was delivered before the batch closed"
        );

        held.close();
        derived.assert_matches(&rows_of(&source), "after the batch closed");
        assert_eq!(source.get_value(&1), Some(30));
    });
}

#[test]
fn a_write_to_a_key_another_thread_is_delivering_follows_it_without_waiting() {
    in_root_and_in_a_child(|domain| {
        let source = domain.enter(CellMap::<u32, i64>::new);
        // Subscribed first, so nothing else has seen the first write while
        // its delivery is parked here.
        let gate = Arc::new(Gate::default());
        let _parked = domain.enter(|| {
            let gate = Arc::clone(&gate);
            source.subscribe_diffs(move |diff| {
                if matches!(diff, MapDiff::Insert { value: 10, .. }) {
                    gate.pass();
                }
            })
        });
        let derived = Derived::of(domain, &source);
        gate.arm();

        let first = {
            let source = source.clone();
            thread::spawn(move || {
                source.insert(1, 10);
            })
        };
        assert!(gate.wait_entered(), "the first write was never delivered");

        // The same key: this thread returns, and the first one delivers.
        source.insert(1, 20);
        assert_eq!(source.remove(&1), Some(20));
        source.insert(1, 30);
        let delivered = derived.selected.get_value(&1);
        assert!(
            delivered.is_none() || delivered == Some(10),
            "a write that follows a parked delivery was delivered first: {delivered:?}"
        );
        // Another key is delivered here and now, beside the parked delivery.
        source.insert(2, 40);
        assert_eq!(derived.selected.get_value(&2), Some(40));

        gate.open();
        assert!(first.join().is_ok());
        assert_eq!(rows_of(&source), Rows::from([(1, 30), (2, 40)]));
        derived.assert_matches(&rows_of(&source), "after the first delivery returned");
    });
}

/// A writer alone on a map delivers without recording its keys in the
/// map's list of holders. Its parked delivery must still be followed by a
/// batch and by another thread's write that arrive while it runs, and the
/// writer must deliver alone again once they have finished.
#[test]
fn a_lone_writer_is_followed_by_writers_that_arrive_during_its_delivery() {
    in_root_and_in_a_child(|domain| {
        let source = domain.enter(CellMap::<u32, i64>::new);
        let gate = Arc::new(Gate::default());
        let delivered: Log<i64> = Arc::default();
        let _parked = domain.enter(|| {
            let (gate, delivered) = (Arc::clone(&gate), Arc::clone(&delivered));
            source.subscribe_diffs(move |diff| {
                if let MapDiff::Insert { key: 1, value }
                | MapDiff::Update {
                    key: 1,
                    new_value: value,
                    ..
                } = diff
                {
                    delivered.lock().push(*value);
                    if *value == 10 {
                        gate.pass();
                    }
                }
            })
        });
        let derived = Derived::of(domain, &source);
        gate.arm();

        let lone = {
            let source = source.clone();
            thread::spawn(move || {
                source.insert(1, 10);
                // Returns once its delivery has, while the batch below is
                // still open on the key.
                source.insert(1, 40);
            })
        };
        assert!(gate.wait_entered(), "the lone write was never delivered");

        let held = HeldBatch::open(domain);
        held.run({
            let source = source.clone();
            move || {
                source.insert(1, 20);
            }
        });
        source.insert(1, 30);
        assert_eq!(*delivered.lock(), vec![10]);

        gate.open();
        assert!(lone.join().is_ok());
        assert_eq!(
            *delivered.lock(),
            vec![10],
            "a write that follows the open batch was delivered before it closed"
        );
        assert_eq!(derived.selected.get_value(&1), Some(10));

        held.close();
        domain.flush();
        assert_eq!(*delivered.lock(), vec![10, 20, 30, 40]);
        derived.assert_matches(&rows_of(&source), "after the batch closed");

        // Nobody holds the map any more, so this write is delivered alone,
        // before it returns.
        source.insert(1, 50);
        assert_eq!(*delivered.lock(), vec![10, 20, 30, 40, 50]);
        assert_eq!(derived.selected.get_value(&1), Some(50));
        derived.assert_matches(&rows_of(&source), "after the lone write");
    });
}

/// One thread writes a key outside any batch, alone on the map at first.
/// Partway through, other threads start writing the same keys, in batches
/// and outside them. Every diff must still follow the row before it.
#[test]
fn a_stream_of_lone_writes_stays_in_order_when_other_writers_join() {
    const STREAM: i64 = 2_000;
    const JOIN_AT: i64 = 500;
    in_root_and_in_a_child(|domain| {
        let source = domain.enter(CellMap::<u32, i64>::new);
        let derived = Derived::of(domain, &source);
        let next_value = Arc::new(AtomicU64::new(1_000_000));
        let joined = Arc::new(Barrier::new(3));

        let stream = {
            let (source, joined) = (source.clone(), Arc::clone(&joined));
            thread::spawn(move || {
                for value in 1..=STREAM {
                    if value == JOIN_AT {
                        joined.wait();
                    }
                    source.insert(0, value);
                }
            })
        };
        let others: Vec<_> = [true, false]
            .into_iter()
            .map(|batched| {
                let (domain, source) = (domain.clone(), source.clone());
                let (next_value, joined) = (Arc::clone(&next_value), Arc::clone(&joined));
                thread::spawn(move || {
                    joined.wait();
                    for _ in 0..200 {
                        let value = || {
                            i64::try_from(next_value.fetch_add(1, Ordering::Relaxed)).unwrap_or(0)
                        };
                        if batched {
                            domain.batch(|| {
                                source.insert(0, value());
                                source.insert(1, value());
                            });
                        } else {
                            source.insert(0, value());
                            source.remove(&1);
                        }
                    }
                })
            })
            .collect();

        assert!(stream.join().is_ok(), "the lone writer panicked");
        for other in others {
            assert!(other.join().is_ok(), "a joining writer panicked");
        }
        domain.flush();
        derived.assert_matches(&rows_of(&source), "after every writer returned");
    });
}

/// One thread's delivery is parked inside a subscriber of a cell that
/// describes the whole map. Another thread then writes a different key, which
/// it delivers itself. The parked cell must not take the second value until
/// its subscribers have returned from the first.
#[test]
fn cells_of_the_whole_map_take_each_value_in_turn() {
    in_root_and_in_a_child(|domain| {
        for park_in_length in [true, false] {
            let source = domain.enter(CellMap::<u32, i64>::new);
            let (size, entries) =
                domain.enter(|| (source.size().materialize(), source.entries().materialize()));
            let gate = Arc::new(Gate::default());
            let sizes: Log<usize> = Log::default();
            let _guards = domain.enter(|| {
                let (length_gate, entries_gate) = (Arc::clone(&gate), Arc::clone(&gate));
                let sizes = Arc::clone(&sizes);
                (
                    size.subscribe(move |signal| {
                        if let hyphae::Signal::Value(len) = signal {
                            sizes.lock().push(**len);
                            if park_in_length && **len == 1 {
                                length_gate.pass();
                            }
                        }
                    }),
                    entries.subscribe(move |signal| {
                        if let hyphae::Signal::Value(rows) = signal
                            && !park_in_length
                            && rows.len() == 1
                        {
                            entries_gate.pass();
                        }
                    }),
                )
            });
            gate.arm();

            let first = {
                let source = source.clone();
                thread::spawn(move || {
                    source.insert(1, 10);
                })
            };
            assert!(gate.wait_entered(), "the first write was never delivered");
            source.insert(2, 20);
            if park_in_length {
                assert_eq!(size.get(), 1, "the length took a value out of turn");
                assert_eq!(entries.get().len(), 2);
            } else {
                assert_eq!(
                    entries.get().len(),
                    1,
                    "the entries took a value out of turn"
                );
            }

            gate.open();
            assert!(first.join().is_ok());
            assert_eq!(size.get(), 2);
            assert_eq!(entries.get().len(), 2);
            assert_eq!(*sizes.lock(), vec![0, 1, 2]);
        }
    });
}

#[test]
fn a_batch_that_rewrites_a_key_after_another_wrote_it_ends_every_observer_on_its_row() {
    in_root_and_in_a_child(|domain| {
        let source = domain.enter(CellMap::<u32, i64>::new);
        let derived = Derived::of(domain, &source);
        let first = HeldBatch::open(domain);
        let write = |batch: &HeldBatch, value: i64| {
            let source = source.clone();
            batch.run(move || {
                source.insert(1, value);
            });
        };

        write(&first, 10);
        let second = HeldBatch::open(domain);
        write(&second, 20);
        second.close();
        write(&first, 30);
        first.close();

        assert_eq!(source.get_value(&1), Some(30));
        derived.assert_matches(&rows_of(&source), "after both batches closed");
    });
}

#[test]
fn two_batches_writing_the_same_keys_in_opposite_orders_commit_without_waiting() {
    in_root_and_in_a_child(|domain| {
        let source = domain.enter(CellMap::<u32, i64>::new);
        let derived = Derived::of(domain, &source);
        let both_wrote_once = Arc::new(Barrier::new(2));

        let writers: Vec<_> = [(1u32, 2u32, 100i64), (2, 1, 200)]
            .into_iter()
            .map(|(first, second, value)| {
                let (domain, source) = (domain.clone(), source.clone());
                let both_wrote_once = Arc::clone(&both_wrote_once);
                thread::spawn(move || {
                    domain.batch(|| {
                        source.insert(first, value);
                        both_wrote_once.wait();
                        source.insert(second, value + 1);
                    });
                })
            })
            .collect();
        for writer in writers {
            assert!(writer.join().is_ok());
        }

        derived.assert_matches(&rows_of(&source), "after both batches closed");
    });
}

#[test]
fn a_batch_that_follows_an_open_batch_stays_atomic() {
    in_root_and_in_a_child(|domain| {
        let (source, flag, pair) = domain.enter(|| {
            let source = CellMap::<u32, i64>::new();
            let flag = Cell::new(0i64);
            let pair = source
                .get(&1)
                .materialize()
                .join(flag.clone())
                .materialize();
            (source, flag, pair)
        });
        let seen: Log<(Option<i64>, i64)> = Log::default();
        let _guard = domain.enter(|| pair.subscribe(log_values(&seen)));

        let held = HeldBatch::open(domain);
        held.run({
            let source = source.clone();
            move || {
                source.insert(1, 10);
            }
        });
        domain.batch(|| {
            source.insert(1, 20);
            flag.set(1);
        });
        assert_eq!(
            pair.get(),
            (None, 0),
            "part of a batch was delivered while a batch it follows was open"
        );

        held.close();
        assert_eq!(*seen.lock(), vec![(None, 0), (Some(20), 1)]);
    });
}

#[test]
fn an_open_batch_does_not_hold_back_writes_to_other_keys() {
    in_root_and_in_a_child(|domain| {
        let source = domain.enter(CellMap::<u32, i64>::new);
        let derived = Derived::of(domain, &source);
        let held = HeldBatch::open(domain);
        held.run({
            let source = source.clone();
            move || {
                source.insert(1, 10);
            }
        });

        domain.batch(|| {
            source.insert(2, 20);
        });
        assert_eq!(derived.selected.get_value(&2), Some(20), "a batched write");
        // An unbatched write made while a batch is open waits for the next
        // drain, as a cell write does. An empty batch runs one.
        source.insert(3, 30);
        domain.batch(|| {});
        assert_eq!(
            derived.selected.get_value(&3),
            Some(30),
            "an unbatched write"
        );
        assert_eq!(derived.size.get(), 2);
        assert_eq!(derived.selected.get_value(&1), None);

        held.close();
        derived.assert_matches(&rows_of(&source), "after the batch closed");
    });
}

#[test]
fn the_length_counts_delivered_rows_when_batches_close_out_of_stored_order() {
    in_root_and_in_a_child(|domain| {
        let source = domain.enter(CellMap::<u32, i64>::new);
        let derived = Derived::of(domain, &source);
        let held = HeldBatch::open(domain);
        held.run({
            let source = source.clone();
            move || {
                source.insert(1, 10);
            }
        });

        domain.batch(|| {
            source.insert(2, 20);
            source.insert(3, 30);
        });
        assert_eq!(
            derived.size.get(),
            2,
            "the open batch's row is not delivered"
        );

        held.close();
        assert_eq!(derived.size.get(), 3);
        derived.assert_matches(&rows_of(&source), "after the batch closed");
    });
}

#[test]
fn a_removal_that_empties_the_map_does_not_deliver_another_batchs_removal() {
    in_root_and_in_a_child(|domain| {
        let source = domain.enter(CellMap::<u32, i64>::new);
        source.insert_many(vec![(1, 10), (2, 20)]);
        let derived = Derived::of(domain, &source);
        let held = HeldBatch::open(domain);
        held.run({
            let source = source.clone();
            move || {
                source.remove(&2);
            }
        });

        domain.batch(|| source.remove_many(vec![1]));
        assert_eq!(
            derived.selected.get_value(&2),
            Some(20),
            "the open batch's removal was delivered by another batch's clearing diff"
        );

        held.close();
        derived.assert_matches(&Rows::new(), "after the batch closed");
    });
}

#[test]
fn applied_diffs_follow_an_open_batch_on_the_keys_they_name() {
    in_root_and_in_a_child(|domain| {
        let source = domain.enter(CellMap::<u32, i64>::new);
        source.insert(3, 30);
        let derived = Derived::of(domain, &source);
        let held = HeldBatch::open(domain);
        held.run({
            let source = source.clone();
            move || {
                source.insert(1, 10);
            }
        });

        // Several keys in one diff, one of them the open batch's.
        domain.batch(|| {
            source.apply_batch(vec![
                MapDiff::Update {
                    key: 1,
                    old_value: 10,
                    new_value: 20,
                },
                MapDiff::Insert { key: 2, value: 21 },
            ]);
        });
        assert_eq!(
            derived.selected.get_value(&2),
            None,
            "a diff of several keys"
        );

        // A replacement of the whole map, nested in a batch of diffs.
        domain.batch(|| {
            source.apply_batch(vec![MapDiff::Initial {
                entries: vec![(4, 40)],
            }]);
        });
        assert_eq!(derived.selected.get_value(&3), Some(30), "a replacement");

        held.close();
        assert_eq!(rows_of(&source), Rows::from([(4, 40)]));
        derived.assert_matches(&rows_of(&source), "after the batch closed");
    });
}

#[test]
fn a_batch_reads_its_own_map_writes_before_it_closes() {
    in_root_and_in_a_child(|domain| {
        let source = domain.enter(CellMap::<u32, i64>::new);
        let derived = Derived::of(domain, &source);
        let hooked: Log<String> = Log::default();
        let _hook = {
            let hooked = Arc::clone(&hooked);
            source.register_write_hook(move |write| {
                hooked.lock().push(match write {
                    MapWrite::Insert { key, value } => format!("insert {key}={value}"),
                    MapWrite::Update { key, old, new } => format!("update {key}={old}>{new}"),
                    MapWrite::Remove { key, old } => format!("remove {key}={old}"),
                });
            })
        };

        domain.batch(|| {
            assert_eq!(source.insert(1, 10), None);
            assert_eq!(source.insert(1, 11), Some(10));
            source.insert_many(vec![(2, 20), (3, 30)]);
            assert_eq!(source.get_value(&1), Some(11));
            assert!(source.contains_key(&3));
            assert_eq!(source.snapshot().len(), 3);
            assert_eq!(source.remove(&2), Some(20));
            assert!(!source.contains_key(&2));
            assert!(!source.is_empty());
            assert_eq!(
                *hooked.lock(),
                [
                    "insert 1=10",
                    "update 1=10>11",
                    "insert 2=20",
                    "insert 3=30",
                    "remove 2=20"
                ]
            );
            assert_eq!(
                derived.size.get(),
                0,
                "the batch was delivered before it closed"
            );
            assert_eq!(derived.selected.snapshot(), vec![]);
        });

        derived.assert_matches(&Rows::from([(1, 11), (3, 30)]), "after the batch closed");
    });
}

#[test]
fn a_child_reading_a_root_map_receives_its_diffs_in_stored_order() {
    let _serial = ROOT_TICK_SERIAL.lock();
    let source = CellMap::<u32, i64>::new();
    let child = Domain::child();
    let derived = Derived::of(&child, &source);
    let root = Domain::root();

    let first = HeldBatch::open(&root);
    let second = HeldBatch::open(&root);
    let write = |batch: &HeldBatch, key: u32, value: i64| {
        let source = source.clone();
        batch.run(move || {
            source.insert(key, value);
        });
    };
    write(&first, 1, 10);
    write(&second, 2, 20);
    write(&second, 1, 21);
    write(&first, 2, 11);
    second.close();
    source.insert(1, 30);
    first.close();

    child.flush();
    assert_eq!(rows_of(&source), Rows::from([(1, 30), (2, 11)]));
    derived.assert_matches(&rows_of(&source), "in the child");
}

#[test]
fn a_set_delivers_its_diffs_in_stored_order() {
    in_root_and_in_a_child(|domain| {
        let set = domain.enter(CellSet::<u32>::new);
        let (len, diffs) = domain.enter(|| (set.len().materialize(), set.diffs().materialize()));
        let members = Arc::new(Mutex::new(std::collections::BTreeSet::new()));
        let _guard = domain.enter(|| {
            let members = Arc::clone(&members);
            diffs.subscribe(move |signal| {
                if let hyphae::Signal::Value(diff) = signal {
                    match diff.as_ref() {
                        Some(SetDiff::Insert(value)) => {
                            members.lock().insert(*value);
                        }
                        Some(SetDiff::Remove(value)) => {
                            members.lock().remove(value);
                        }
                        None => {}
                    }
                }
            })
        });

        let held = HeldBatch::open(domain);
        held.run({
            let set = set.clone();
            move || {
                set.insert(1);
            }
        });
        domain.batch(|| {
            set.remove(&1);
            set.insert(2);
        });
        set.insert(3);
        domain.batch(|| {});
        assert_eq!(
            len.get(),
            1,
            "only the write to an untouched value is delivered"
        );
        held.close();

        assert!(!set.contains_value(&1));
        assert_eq!(*members.lock(), [2, 3].into());
        assert_eq!(len.get(), 2);
    });
}

#[test]
fn a_sets_length_takes_each_value_in_turn() {
    in_root_and_in_a_child(|domain| {
        let set = domain.enter(CellSet::<u32>::new);
        let len = domain.enter(|| set.len().materialize());
        let gate = Arc::new(Gate::default());
        let seen: Log<usize> = Log::default();
        let _guard = domain.enter(|| {
            let (gate, seen) = (Arc::clone(&gate), Arc::clone(&seen));
            len.subscribe(move |signal| {
                if let hyphae::Signal::Value(len) = signal {
                    seen.lock().push(**len);
                    if **len == 1 {
                        gate.pass();
                    }
                }
            })
        });
        gate.arm();

        let first = {
            let set = set.clone();
            thread::spawn(move || {
                set.insert(1);
            })
        };
        assert!(gate.wait_entered(), "the first insert was never delivered");
        set.insert(2);
        assert_eq!(len.get(), 1, "the length took a value out of turn");

        gate.open();
        assert!(first.join().is_ok());
        assert_eq!(len.get(), 2);
        assert_eq!(*seen.lock(), vec![0, 1, 2]);
    });
}

#[test]
fn a_sets_values_follow_every_write_of_one_batch() {
    in_root_and_in_a_child(|domain| {
        let set = domain.enter(CellSet::<u32>::new);
        let values = domain.enter(|| set.values().materialize());

        domain.batch(|| {
            set.insert(1);
            set.insert(2);
            set.insert(3);
            set.remove(&2);
        });

        assert_eq!(values.get(), vec![1, 3]);
    });
}

/// A small deterministic generator, so a failing seed replays.
struct Rng(u64);

impl Rng {
    const fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, bound: u32) -> u32 {
        u32::try_from(self.next() % u64::from(bound)).unwrap_or(0)
    }
}

/// One random write. Every value is unique, so the replay can tell any two
/// rows of a key apart.
fn write(source: &CellMap<u32, i64>, rng: &mut Rng, next_value: &AtomicU64) {
    let value = || i64::try_from(next_value.fetch_add(1, Ordering::Relaxed)).unwrap_or(0);
    let key = rng.below(KEYS);
    match rng.below(20) {
        0..=9 => {
            source.insert(key, value());
        }
        10..=13 => {
            source.remove(&key);
        }
        14..=15 => source.insert_many(vec![(key, value()), ((key + 1) % KEYS, value())]),
        16..=17 => source.remove_many(vec![key, (key + 2) % KEYS]),
        18 => source.replace_all(vec![(key, value()), ((key + 3) % KEYS, value())]),
        _ => source.remove_many((0..KEYS).collect()),
    }
}

const WRITERS: u64 = 6;
const ROUNDS: u32 = 40;

/// Six threads write the same few keys, half of the time inside batches of
/// several writes, and every derived structure is compared with the source
/// once they have all returned.
fn stress(domain: &Domain, reader: &Domain, seed: u64) {
    let source = domain.enter(CellMap::<u32, i64>::new);
    let derived = Derived::of(reader, &source);
    let next_value = Arc::new(AtomicU64::new(1));
    let start = Arc::new(Barrier::new(usize::try_from(WRITERS).unwrap_or(1)));

    let writers: Vec<_> = (0..WRITERS)
        .map(|writer| {
            let (domain, source) = (domain.clone(), source.clone());
            let (next_value, start) = (Arc::clone(&next_value), Arc::clone(&start));
            thread::spawn(move || {
                let mut rng = Rng((seed << 8 | writer).wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
                start.wait();
                for _ in 0..ROUNDS {
                    if rng.below(2) == 0 {
                        write(&source, &mut rng, &next_value);
                        continue;
                    }
                    domain.batch(|| {
                        for _ in 0..=rng.below(4) {
                            write(&source, &mut rng, &next_value);
                            if rng.below(3) == 0 {
                                thread::yield_now();
                            }
                        }
                    });
                }
            })
        })
        .collect();
    for writer in writers {
        assert!(writer.join().is_ok(), "seed {seed}: a writer panicked");
    }
    domain.flush();
    reader.flush();

    derived.assert_matches(&rows_of(&source), &format!("seed {seed}"));
}

const SEEDS: u64 = 150;

#[test]
fn derived_structures_match_a_root_map_after_mixed_writers() {
    let _serial = ROOT_TICK_SERIAL.lock();
    let root = Domain::root();
    for seed in 0..SEEDS {
        stress(&root, &root, seed);
    }
}

#[test]
fn derived_structures_match_a_child_map_after_mixed_writers() {
    let _serial = ROOT_TICK_SERIAL.lock();
    let child = Domain::child();
    for seed in 0..SEEDS {
        stress(&child, &child, seed);
    }
}

#[test]
fn a_childs_derived_structures_match_a_root_map_after_mixed_writers() {
    let _serial = ROOT_TICK_SERIAL.lock();
    let root = Domain::root();
    let child = Domain::child();
    for seed in 0..SEEDS {
        stress(&root, &child, seed);
    }
}
