//! A collection's diff stream when its writers are inside event sets.
//!
//! A mutation made inside `batch`, `submit`, or `enqueue` is queued when the
//! event set closes, which can be after another thread's later mutation.
//! These tests check what a subscriber is told in that case.
//!
//! The wave switches are process-wide, so the tests take turns, and each one
//! runs with waves off and with waves on.

use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    thread,
};

use hyphae::{
    Cell, CellMap, CellSet, Gettable, MapDiff, MapExt, Materialize, Mutable, SetDiff, Signal,
    SubscriptionGuard, Watchable, batch, enqueue, flush, settle, submit,
};
use parking_lot::Mutex;

const KEYS: [&str; 3] = ["a", "b", "c"];
const WRITERS: u64 = 4;
const ROUNDS: u64 = 5_000;

static TURN: Mutex<()> = Mutex::new(());

/// Runs `scenario` with every wave on the holder, then with every wave of
/// two or more nodes on eight threads.
fn in_both_modes(scenario: impl Fn()) {
    let _turn = TURN.lock();
    settle::set_wave_min(1);
    for workers in [0, 8] {
        settle::set_workers(workers);
        scenario();
    }
    settle::set_workers(0);
}

/// How a writer submits an event set.
#[derive(Clone, Copy, Debug)]
enum Entry {
    Batch,
    Submit,
    Enqueue,
}

const ENTRIES: [Entry; 3] = [Entry::Batch, Entry::Submit, Entry::Enqueue];

impl Entry {
    fn write(self, writes: impl FnOnce()) {
        match self {
            Self::Batch => batch(writes),
            Self::Submit => submit(&[], None, writes),
            Self::Enqueue => enqueue(&[], None, writes),
        }
    }
}

/// What a subscriber that replays every diff holds, and how many diffs did
/// not follow from what it held.
#[derive(Default)]
struct Replayed {
    held: Mutex<HashMap<&'static str, u64>>,
    mismatches: AtomicUsize,
}

impl Replayed {
    fn subscribe(map: &CellMap<&'static str, u64>) -> (Arc<Self>, SubscriptionGuard) {
        let replayed = Arc::new(Self::default());
        let sink = replayed.clone();
        let guard = map.subscribe_diffs(move |diff| sink.replay(diff));
        (replayed, guard)
    }

    fn replay(&self, diff: &MapDiff<&'static str, u64>) {
        let follows = match diff {
            MapDiff::Initial { entries } => {
                *self.held.lock() = entries.iter().copied().collect();
                true
            }
            MapDiff::Insert { key, value } => self.held.lock().insert(key, *value).is_none(),
            MapDiff::Update {
                key,
                old_value,
                new_value,
            } => self.held.lock().insert(key, *new_value) == Some(*old_value),
            MapDiff::Remove { key, old_value } => self.held.lock().remove(key) == Some(*old_value),
            MapDiff::Batch { changes } => {
                for change in changes {
                    self.replay(change);
                }
                true
            }
        };
        if !follows {
            self.mismatches.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn assert_holds(&self, map: &CellMap<&'static str, u64>, context: &str) {
        assert_eq!(
            self.mismatches.load(Ordering::SeqCst),
            0,
            "{context}: a diff did not follow from the diffs before it"
        );
        let stored: HashMap<_, _> = map.snapshot().into_iter().collect();
        assert_eq!(*self.held.lock(), stored, "{context}");
    }
}

/// `WRITERS` threads that each insert and remove the same keys, every
/// mutation inside its own event set.
fn race_writers(map: &CellMap<&'static str, u64>, entry: Entry) {
    let writers: Vec<_> = (0..WRITERS)
        .map(|writer| {
            let map = map.clone();
            thread::spawn(move || {
                for round in 0..ROUNDS {
                    let turn = usize::try_from(round.wrapping_add(writer)).unwrap_or(0);
                    let key = KEYS.iter().cycle().nth(turn % 3).copied().unwrap_or("a");
                    entry.write(|| {
                        if round % 5 == 3 {
                            map.remove(&key);
                        } else {
                            map.insert(key, round.wrapping_mul(WRITERS).wrapping_add(writer));
                        }
                        // Leave room for another thread's mutation between
                        // this one and the close of the event set.
                        thread::yield_now();
                    });
                }
            })
        })
        .collect();
    for writer in writers {
        assert!(writer.join().is_ok());
    }
    flush();
}

#[test]
fn writers_inside_event_sets_leave_a_diff_subscriber_with_the_map() {
    in_both_modes(|| {
        for entry in ENTRIES {
            let map = CellMap::<&'static str, u64>::new();
            let (replayed, _guard) = Replayed::subscribe(&map);
            race_writers(&map, entry);
            replayed.assert_holds(&map, &format!("{entry:?}"));
        }
    });
}

#[test]
fn writers_inside_event_sets_leave_the_key_cells_and_the_length_at_the_map() {
    in_both_modes(|| {
        for entry in ENTRIES {
            let map = CellMap::<&'static str, u64>::new();
            let cells = KEYS.map(|key| map.get(&key).materialize());
            let len = map.len().materialize();
            race_writers(&map, entry);
            for (key, cell) in KEYS.iter().zip(&cells) {
                assert_eq!(cell.get(), map.get_value(key), "{entry:?} key {key}");
            }
            assert_eq!(len.get(), map.snapshot().len(), "{entry:?}");
        }
    });
}

#[test]
fn writers_inside_event_sets_leave_a_diff_subscriber_with_the_set() {
    in_both_modes(|| {
        for entry in ENTRIES {
            let set = CellSet::<u64>::new();
            let held = Arc::new(Mutex::new(HashSet::new()));
            let mismatches = Arc::new(AtomicUsize::new(0));
            let (sink, counted) = (held.clone(), mismatches.clone());
            let diffs = set.diffs().materialize();
            let _guard = diffs.subscribe(move |signal| {
                let Signal::Value(diff) = signal else { return };
                let follows = match diff.as_ref() {
                    None => true,
                    Some(SetDiff::Insert(value)) => sink.lock().insert(*value),
                    Some(SetDiff::Remove(value)) => sink.lock().remove(value),
                };
                if !follows {
                    counted.fetch_add(1, Ordering::SeqCst);
                }
            });
            let members = [1u64, 2].map(|value| set.contains(&value).materialize());
            let len = set.len().materialize();
            let writers: Vec<_> = (0..WRITERS)
                .map(|writer| {
                    let set = set.clone();
                    thread::spawn(move || {
                        for round in 0..ROUNDS {
                            let value = 1 + round % 2;
                            entry.write(|| {
                                if (round / 2 + writer) % 2 == 0 {
                                    set.insert(value);
                                } else {
                                    set.remove(&value);
                                }
                                thread::yield_now();
                            });
                        }
                    })
                })
                .collect();
            for writer in writers {
                assert!(writer.join().is_ok());
            }
            flush();
            assert_eq!(mismatches.load(Ordering::SeqCst), 0, "{entry:?}");
            for (value, member) in [1u64, 2].iter().zip(&members) {
                assert_eq!(held.lock().contains(value), set.contains_value(value));
                assert_eq!(member.get(), set.contains_value(value), "{entry:?}");
            }
            assert_eq!(len.get(), held.lock().len(), "{entry:?}");
        }
    });
}

#[test]
fn bulk_writers_that_empty_and_refill_the_map_leave_a_diff_subscriber_with_the_map() {
    in_both_modes(|| {
        for entry in ENTRIES {
            let map = CellMap::<&'static str, u64>::new();
            let (replayed, _guard) = Replayed::subscribe(&map);
            let filler = map.clone();
            let fill = thread::spawn(move || {
                for round in 0..ROUNDS {
                    // The same values each time, so a refill of a full map
                    // changes nothing.
                    let entries: Vec<_> = KEYS.iter().map(|key| (*key, round / 64)).collect();
                    entry.write(|| filler.insert_many(entries));
                }
            });
            let emptier = map.clone();
            let empty = thread::spawn(move || {
                for _ in 0..ROUNDS {
                    for key in KEYS {
                        // Removing the last key emits `MapDiff::Initial`.
                        entry.write(|| emptier.remove_many(vec![key]));
                    }
                }
            });
            assert!(fill.join().is_ok());
            assert!(empty.join().is_ok());
            flush();
            replayed.assert_holds(&map, &format!("{entry:?}"));
        }
    });
}

#[test]
fn a_publish_applied_by_a_thread_that_has_a_batch_open_keeps_the_stream_exact() {
    in_both_modes(|| {
        let map = CellMap::<&'static str, u64>::new();
        let set = CellSet::<u64>::new();
        let (replayed, _guard) = Replayed::subscribe(&map);
        let member = set.contains(&1).materialize();
        enqueue(&[], None, || {
            map.insert("key", 1);
            set.insert(1);
        });
        batch(|| {
            // This thread settles the queued event set while its own batch
            // is open, so nothing it writes now is delivered before the
            // batch closes.
            flush();
            let (map, set) = (map.clone(), set.clone());
            let later = thread::spawn(move || {
                map.insert("key", 2);
                set.remove(&1);
            });
            assert!(later.join().is_ok());
        });
        flush();
        replayed.assert_holds(&map, "flush inside a batch");
        assert_eq!(member.get(), set.contains_value(&1));
    });
}

/// Counts the values `cell` delivers after subscription.
fn count_deliveries<T: hyphae::CellValue>(
    cell: &impl Watchable<T>,
) -> (Arc<AtomicUsize>, SubscriptionGuard) {
    let count = Arc::new(AtomicUsize::new(0));
    let sink = count.clone();
    let first = AtomicBool::new(true);
    let guard = cell.subscribe(move |signal| {
        if !first.swap(false, Ordering::SeqCst) && matches!(signal, Signal::Value(_)) {
            sink.fetch_add(1, Ordering::SeqCst);
        }
    });
    (count, guard)
}

#[test]
fn an_insert_and_its_remove_in_one_batch_tell_nobody() {
    in_both_modes(|| {
        let map = CellMap::<&'static str, u64>::new();
        map.insert("kept", 1);
        let (diffs, _diffs) = count_deliveries(&map.diffs().materialize());
        let (key, _key) = count_deliveries(&map.get(&"gone").materialize());
        let (len, _len) = count_deliveries(&map.len().materialize());
        batch(|| {
            map.insert("gone", 2);
            map.remove(&"gone");
        });
        assert_eq!(diffs.load(Ordering::SeqCst), 0);
        assert_eq!(key.load(Ordering::SeqCst), 0);
        assert_eq!(len.load(Ordering::SeqCst), 0);
    });
}

#[test]
fn two_writes_of_one_key_in_one_batch_tell_one_diff() {
    in_both_modes(|| {
        let map = CellMap::<&'static str, u64>::new();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        let _guard = map.subscribe_diffs(move |diff| sink.lock().push(diff.clone()));
        batch(|| {
            map.insert("key", 1);
            map.insert("key", 2);
        });
        assert_eq!(
            *seen.lock(),
            vec![
                MapDiff::Initial { entries: vec![] },
                MapDiff::Insert {
                    key: "key",
                    value: 2
                }
            ]
        );
    });
}

#[test]
fn a_callback_that_writes_a_key_another_thread_has_in_flight_keeps_the_stream_exact() {
    in_both_modes(|| {
        let source = Cell::new(0u64);
        let map = CellMap::<&'static str, u64>::new();
        let derived = source.clone().map(|value| *value).materialize();
        let target = map.clone();
        let first = AtomicBool::new(true);
        map.own(derived.subscribe(move |signal| {
            if let Signal::Value(value) = signal
                && !first.swap(false, Ordering::SeqCst)
            {
                target.insert("key", **value);
            }
        }));
        let (replayed, _guard) = Replayed::subscribe(&map);

        let (wrote, written) = mpsc::channel();
        let (close, closing) = mpsc::channel::<()>();
        let writer = map.clone();
        let other = thread::spawn(move || {
            batch(|| {
                writer.insert("key", 1);
                assert!(wrote.send(()).is_ok());
                assert!(closing.recv().is_ok());
            });
        });
        assert!(written.recv().is_ok());
        // The map holds 1 and no subscriber has been told. This settle's
        // callback writes the same key.
        source.set(7);
        assert_eq!(map.get_value(&"key"), Some(7));
        assert!(close.send(()).is_ok());
        assert!(other.join().is_ok());
        flush();
        replayed.assert_holds(&map, "callback write");
    });
}

#[test]
fn an_initial_applied_outside_a_settle_tells_the_stored_entries() {
    in_both_modes(|| {
        let map = CellMap::<&'static str, u64>::new();
        map.insert("a", 1);
        map.insert("b", 2);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        let _diffs = map.diffs().materialize().subscribe(move |signal| {
            if let Signal::Value(diff) = signal {
                sink.lock().push((**diff).clone());
            }
        });
        let (replayed, _guard) = Replayed::subscribe(&map);
        let gone = map.get(&"a").materialize();
        let len = map.len().materialize();

        map.apply_diff_owned(MapDiff::Initial {
            entries: vec![("c", 3)],
        });
        assert_eq!(
            seen.lock().last(),
            Some(&MapDiff::Initial {
                entries: vec![("c", 3)]
            })
        );
        assert_eq!(gone.get(), None);
        assert_eq!(len.get(), 1);
        replayed.assert_holds(&map, "initial");
    });
}

#[test]
fn an_initial_applied_while_another_thread_has_a_key_in_flight_keeps_the_stream_exact() {
    in_both_modes(|| {
        let map = CellMap::<&'static str, u64>::new();
        map.insert("a", 1);
        let (replayed, _guard) = Replayed::subscribe(&map);
        let cells = KEYS.map(|key| map.get(&key).materialize());

        let (wrote, written) = mpsc::channel();
        let (close, closing) = mpsc::channel::<()>();
        let writer = map.clone();
        let other = thread::spawn(move || {
            batch(|| {
                writer.insert("a", 5);
                writer.insert("b", 6);
                assert!(wrote.send(()).is_ok());
                assert!(closing.recv().is_ok());
                writer.insert("c", 8);
            });
        });
        assert!(written.recv().is_ok());
        map.apply_diff_owned(MapDiff::Initial {
            entries: vec![("b", 7)],
        });
        replayed.assert_holds(&map, "initial before the batch closes");
        assert!(close.send(()).is_ok());
        assert!(other.join().is_ok());
        flush();
        replayed.assert_holds(&map, "initial after the batch closes");
        for (key, cell) in KEYS.iter().zip(&cells) {
            assert_eq!(cell.get(), map.get_value(key), "key {key}");
        }
    });
}

#[test]
fn a_key_cell_made_while_its_key_is_in_flight_ends_at_the_map() {
    in_both_modes(|| {
        let map = CellMap::<&'static str, u64>::new();
        map.insert("key", 5);
        let (wrote, written) = mpsc::channel();
        let (close, closing) = mpsc::channel::<()>();
        let writer = map.clone();
        let other = thread::spawn(move || {
            batch(|| {
                writer.insert("key", 7);
                assert!(wrote.send(()).is_ok());
                assert!(closing.recv().is_ok());
                // Back to what subscribers hold: the batch tells nothing.
                writer.insert("key", 5);
            });
        });
        assert!(written.recv().is_ok());
        let cell = map.get(&"key").materialize();
        assert!(close.send(()).is_ok());
        assert!(other.join().is_ok());
        flush();
        assert_eq!(cell.get(), Some(5));
    });
}
