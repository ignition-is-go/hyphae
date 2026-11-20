//! Operators and collections when nodes of one height fire at the same time.
//!
//! Every test turns waves on for the whole process: each wave of two or more
//! nodes is fired by eight threads.

use std::{
    collections::HashMap,
    sync::{
        Arc, Once,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
};

use hyphae::{
    Cell, CellMap, CellSet, Gettable, JoinExt, MapDiff, MapExt, MapQuery, Materialize, MergeExt,
    Mutable, ProjectCellExt, ScanExt, SetDiff, Signal, TakeUntilExt, Watchable,
    ZipExt, batch, flush, join_vec,
};
use parking_lot::Mutex;

const ROUNDS: u64 = 2_000;

fn waves_on() {
    static ON: Once = Once::new();
    ON.call_once(|| {
        hyphae::settle::set_wave_min(1);
        hyphae::settle::set_workers(8);
    });
}

/// Records every signal a cell delivers after subscription.
fn record<T: hyphae::CellValue>(
    cell: &impl Watchable<T>,
) -> (Arc<Mutex<Vec<Option<T>>>>, hyphae::SubscriptionGuard) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    let first = AtomicBool::new(true);
    let guard = cell.subscribe(move |signal| {
        if first.swap(false, Ordering::SeqCst) {
            return;
        }
        match signal {
            Signal::Value(value) => sink.lock().push(Some((**value).clone())),
            Signal::Complete => sink.lock().push(None),
            Signal::Error(_) => {}
        }
    });
    (seen, guard)
}

/// A cell one step above `source`, so two of them share a height.
fn above(source: &Cell<u64, hyphae::CellMutable>) -> Cell<u64, hyphae::CellImmutable> {
    source.clone().map(|value| *value).materialize()
}

fn replay<K, V>(state: &mut HashMap<K, V>, diff: &MapDiff<K, V>)
where
    K: std::hash::Hash + Eq + Clone,
    V: Clone,
{
    match diff {
        MapDiff::Initial { entries } => {
            state.clear();
            state.extend(entries.iter().cloned());
        }
        MapDiff::Insert { key, value } => {
            state.insert(key.clone(), value.clone());
        }
        MapDiff::Update { key, new_value, .. } => {
            state.insert(key.clone(), new_value.clone());
        }
        MapDiff::Remove { key, .. } => {
            state.remove(key);
        }
        MapDiff::Batch { changes } => {
            for change in changes {
                replay(state, change);
            }
        }
    }
}

#[test]
fn a_join_of_two_cells_that_fire_together_ends_with_both() {
    waves_on();
    let (left, right) = (Cell::new(0u64), Cell::new(0u64));
    let joined = above(&left).join(above(&right)).materialize();
    let (seen, _guard) = record(&joined);
    for round in 1..=ROUNDS {
        batch(|| {
            left.set(round);
            right.set(round);
        });
        assert_eq!(joined.get(), (round, round));
        assert_eq!(seen.lock().last(), Some(&Some((round, round))));
    }
    // One delivery per settle, never one side alone.
    assert_eq!(seen.lock().len(), usize::try_from(ROUNDS).unwrap_or(0));
}

#[test]
fn a_join_vec_of_cells_that_fire_together_ends_with_all() {
    waves_on();
    let inputs: Vec<_> = (0..16).map(|_| Cell::new(0u64)).collect();
    let joined = join_vec(inputs.iter().map(above).collect()).materialize();
    let (seen, _guard) = record(&joined);
    for round in 1..=ROUNDS {
        batch(|| {
            for input in &inputs {
                input.set(round);
            }
        });
        assert_eq!(joined.get(), vec![round; 16]);
    }
    assert_eq!(seen.lock().len(), usize::try_from(ROUNDS).unwrap_or(0));
}

#[test]
fn a_merge_delivers_in_write_order_and_never_runs_downstream_twice_at_once() {
    waves_on();
    let (left, right) = (Cell::new(0u64), Cell::new(0u64));
    let inside = AtomicUsize::new(0);
    let overlapped = Arc::new(AtomicBool::new(false));
    let overlap = overlapped.clone();
    let merged = above(&left)
        .merge(above(&right))
        .scan(Vec::new(), move |trail: &Vec<u64>, value| {
            if inside.fetch_add(1, Ordering::SeqCst) != 0 {
                overlap.store(true, Ordering::SeqCst);
            }
            thread::yield_now();
            let mut trail = trail.clone();
            trail.push(*value);
            inside.fetch_sub(1, Ordering::SeqCst);
            trail
        })
        .materialize();
    let mut expected = merged.get();
    for round in 1..=ROUNDS {
        batch(|| {
            left.set(round * 2);
            right.set(round * 2 + 1);
        });
        expected.extend([round * 2, round * 2 + 1]);
    }
    assert!(!overlapped.load(Ordering::SeqCst));
    assert_eq!(merged.get(), expected);
}

#[test]
fn a_zip_pairs_in_order_when_both_sides_fire_together() {
    waves_on();
    let (left, right) = (Cell::new(0u64), Cell::new(0u64));
    let zipped = above(&left).zip(above(&right)).materialize();
    let (seen, _guard) = record(&zipped);
    let mut expected = Vec::new();
    for round in 1..=ROUNDS {
        batch(|| {
            left.set(round);
            right.set(round + 1_000_000);
        });
        expected.push(Some((round, round + 1_000_000)));
    }
    assert_eq!(*seen.lock(), expected);
}

#[test]
fn a_take_until_passes_the_value_written_before_the_notifier() {
    waves_on();
    for round in 1..=ROUNDS {
        let (source, notifier) = (Cell::new(0u64), Cell::new(0u64));
        let taken = above(&source).take_until(above(&notifier)).materialize();
        let (seen, _guard) = record(&taken);
        batch(|| {
            source.set(round);
            notifier.set(1);
        });
        assert_eq!(*seen.lock(), vec![Some(round), None]);
    }
}

/// Counts the diffs that do not follow from the state the diffs before them
/// left, and keeps that state.
fn check_then_replay(
    state: &mut Option<u64>,
    mismatches: &AtomicUsize,
    diff: &MapDiff<&'static str, u64>,
) {
    let follows = match diff {
        MapDiff::Initial { entries } => {
            *state = entries.first().map(|(_, value)| *value);
            true
        }
        MapDiff::Insert { value, .. } => state.replace(*value).is_none(),
        MapDiff::Update {
            old_value,
            new_value,
            ..
        } => state.replace(*new_value) == Some(*old_value),
        MapDiff::Remove { old_value, .. } => state.take() == Some(*old_value),
        MapDiff::Batch { changes } => {
            for change in changes {
                check_then_replay(state, mismatches, change);
            }
            true
        }
    };
    if !follows {
        mismatches.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn two_threads_that_write_one_key_leave_subscribers_with_the_map_state() {
    waves_on();
    let map = CellMap::<&'static str, u64>::new();
    let replayed = Arc::new(Mutex::new(None));
    let mismatches = Arc::new(AtomicUsize::new(0));
    let (sink, counted) = (replayed.clone(), mismatches.clone());
    let _guard =
        map.subscribe_diffs(move |diff| check_then_replay(&mut sink.lock(), &counted, diff));
    let writers: Vec<_> = (0..4u64)
        .map(|writer| {
            let map = map.clone();
            thread::spawn(move || {
                for round in 0..50_000u64 {
                    if round % 7 == 3 {
                        map.remove(&"key");
                    } else {
                        map.insert("key", round * 4 + writer);
                    }
                }
            })
        })
        .collect();
    for writer in writers {
        assert!(writer.join().is_ok());
    }
    flush();
    // Every diff follows from the one before it, so a subscriber that
    // replays them holds what the map holds.
    assert_eq!(mismatches.load(Ordering::SeqCst), 0);
    assert_eq!(*replayed.lock(), map.get_value(&"key"));
}

#[test]
fn two_cells_of_one_height_that_write_one_key_leave_subscribers_with_the_map_state() {
    waves_on();
    let (left, right) = (Cell::new(0u64), Cell::new(0u64));
    let map = CellMap::<&'static str, u64>::new();
    for source in [above(&left), above(&right)] {
        let target = map.clone();
        let first = AtomicBool::new(true);
        map.own(source.subscribe(move |signal| {
            if let Signal::Value(value) = signal
                && !first.swap(false, Ordering::SeqCst)
            {
                target.insert("key", **value);
            }
        }));
    }
    let replayed = Arc::new(Mutex::new(HashMap::new()));
    let sink = replayed.clone();
    let _guard = map.subscribe_diffs(move |diff| replay(&mut sink.lock(), diff));
    for round in 1..=ROUNDS * 5 {
        batch(|| {
            left.set(round * 2);
            right.set(round * 2 + 1);
        });
        assert_eq!(replayed.lock().get("key").copied(), map.get_value(&"key"));
    }
}

#[test]
fn two_threads_that_write_one_element_leave_subscribers_with_the_set_state() {
    waves_on();
    let set = CellSet::<u64>::new();
    let member = Arc::new(AtomicBool::new(false));
    let mismatches = Arc::new(AtomicUsize::new(0));
    let (sink, counted) = (member.clone(), mismatches.clone());
    let diffs = set.diffs().materialize();
    let _guard = diffs.subscribe(move |signal| {
        let Signal::Value(diff) = signal else { return };
        let follows = match diff.as_ref() {
            None => true,
            Some(SetDiff::Insert(_)) => !sink.swap(true, Ordering::SeqCst),
            Some(SetDiff::Remove(_)) => sink.swap(false, Ordering::SeqCst),
        };
        if !follows {
            counted.fetch_add(1, Ordering::SeqCst);
        }
    });
    let writers: Vec<_> = (0..4u64)
        .map(|writer| {
            let set = set.clone();
            thread::spawn(move || {
                for round in 0..50_000u64 {
                    if (round + writer) % 2 == 0 {
                        set.insert(1);
                    } else {
                        set.remove(&1);
                    }
                }
            })
        })
        .collect();
    for writer in writers {
        assert!(writer.join().is_ok());
    }
    flush();
    assert_eq!(mismatches.load(Ordering::SeqCst), 0);
    assert_eq!(member.load(Ordering::SeqCst), set.contains_value(&1));
}

#[test]
fn row_updates_and_a_source_change_in_one_settle_leave_the_projection_exact() {
    const ROWS: u64 = 64;
    waves_on();
    let rows = CellMap::<u64, u64>::new();
    let weights = [Cell::new(1u64), Cell::new(1u64)];
    for key in 0..ROWS {
        rows.insert(key, key);
    }
    let [even, odd] = weights.each_ref().map(above);
    let projected = rows
        .clone()
        .project_cell(move |key, value| {
            let (key, value) = (*key, *value);
            let weight = if key % 2 == 0 { &even } else { &odd };
            weight
                .clone()
                .map(move |weight| Some((key, value * weight)))
                .materialize()
        })
        .materialize();
    let replayed = Arc::new(Mutex::new(HashMap::new()));
    let sink = replayed.clone();
    let _guard = projected.subscribe_diffs(move |diff| replay(&mut sink.lock(), diff));

    for round in 2..200u64 {
        batch(|| {
            for weight in &weights {
                weight.set(round);
            }
            rows.insert(round % ROWS, round);
        });
        let mut base: HashMap<u64, u64> = (0..ROWS).map(|key| (key, key)).collect();
        for earlier in 2..=round {
            base.insert(earlier % ROWS, earlier);
        }
        let expected: HashMap<u64, u64> = base
            .into_iter()
            .map(|(key, value)| (key, value * round))
            .collect();
        assert_eq!(*replayed.lock(), expected, "round {round}");
        let stored: HashMap<u64, u64> = projected.snapshot().into_iter().collect();
        assert_eq!(stored, expected, "round {round}");
    }
}

#[test]
fn a_mutation_built_from_another_mutation_of_the_same_map_does_not_wait_on_itself() {
    waves_on();
    let map = CellMap::<&'static str, u64>::new();
    let (done, finished) = std::sync::mpsc::channel();
    let writer = map.clone();
    thread::spawn(move || {
        // `apply_diff_owned` applies a batch through `apply_batch`.
        writer.apply_diff_owned(MapDiff::Batch {
            changes: vec![MapDiff::Insert {
                key: "key",
                value: 1,
            }],
        });
        let _ = done.send(());
    });
    assert!(
        finished
            .recv_timeout(std::time::Duration::from_secs(10))
            .is_ok()
    );
    assert_eq!(map.get_value(&"key"), Some(1));
}
