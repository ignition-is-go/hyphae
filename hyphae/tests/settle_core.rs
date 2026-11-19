//! Behavior of the settle core, as a caller can observe it.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use hyphae::{
    Cell, CellMap, Epoch, Gettable, JoinExt, MapDiff, MapExt, MapQuery, Materialize, MergeExt,
    Mutable, ProjectCellExt, SettleKey, Signal, Source, Watchable, batch, batch_with, enqueue,
    flush, submit,
};
use parking_lot::Mutex;

/// Records every value a cell delivers after subscription.
fn record<T: hyphae::CellValue>(
    cell: &impl Watchable<T>,
) -> (Arc<Mutex<Vec<T>>>, hyphae::SubscriptionGuard) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    let first = AtomicBool::new(true);
    let guard = cell.subscribe(move |signal| {
        if first.swap(false, Ordering::SeqCst) {
            return;
        }
        if let Signal::Value(value) = signal {
            sink.lock().push((**value).clone());
        }
    });
    (seen, guard)
}

/// A subscriber that blocks the barrier settle it runs in until released, so
/// a test can queue event sets behind it.
struct HeldSettle {
    gate: Cell<u64, hyphae::CellMutable>,
    release: mpsc::Sender<()>,
    holder: Option<thread::JoinHandle<()>>,
    _guard: hyphae::SubscriptionGuard,
}

impl HeldSettle {
    fn start() -> Self {
        let gate = Cell::new(0u64);
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release, release_rx) = mpsc::channel::<()>();
        let release_rx = Mutex::new(release_rx);
        let guard = gate.subscribe(move |signal| {
            if matches!(signal, Signal::Value(value) if **value == 1) {
                let _ = entered_tx.send(());
                let _ = release_rx.lock().recv_timeout(Duration::from_secs(10));
            }
        });
        let writer = gate.clone();
        // A barrier, so every later write waits for it whatever it touches.
        let holder = thread::spawn(move || {
            batch_with(&[SettleKey::Barrier], None, || writer.set(1));
        });
        assert!(
            entered_rx.recv_timeout(Duration::from_secs(10)).is_ok(),
            "the holding settle must start"
        );
        Self {
            gate,
            release,
            holder: Some(holder),
            _guard: guard,
        }
    }

    fn release(mut self) {
        let _ = self.release.send(());
        if let Some(holder) = self.holder.take() {
            assert!(holder.join().is_ok());
        }
        flush();
        drop(self.gate.clone());
    }
}

#[test]
fn a_diamond_fires_its_join_once_per_settle_with_both_legs_settled() {
    let source = Cell::new(1i64);
    let left = source.clone().map(|value| value * 2).materialize();
    let right = source.clone().map(|value| value * 3).materialize();
    let joined = left.join(right).materialize();
    let (seen, _guard) = record(&joined);

    source.set(2);
    batch(|| {
        source.set(3);
        source.set(4);
    });

    // One delivery per settle, and never a pair built from two source values.
    assert_eq!(*seen.lock(), vec![(4, 6), (8, 12)]);
}

#[test]
fn a_cell_fires_at_most_once_per_settle() {
    let inputs: Vec<_> = (0..8).map(|_| Cell::new(0u64)).collect();
    let sum = hyphae::join_vec(inputs.clone())
        .map(|values| values.iter().sum::<u64>())
        .materialize();
    let fires = Arc::new(AtomicUsize::new(0));
    let counter = fires.clone();
    let _guard = sum.subscribe(move |_| {
        counter.fetch_add(1, Ordering::SeqCst);
    });
    fires.store(0, Ordering::SeqCst);

    let before = hyphae::settle::stats();
    batch(|| {
        for (index, input) in inputs.iter().enumerate() {
            input.set(u64::try_from(index).unwrap_or(0) + 1);
        }
    });
    let after = hyphae::settle::stats();

    assert_eq!(fires.load(Ordering::SeqCst), 1);
    assert_eq!(sum.get(), 36);
    // Other tests in this process settle too, so only a lower bound on runs
    // is exact here; the single delivery above is the per-cell assertion.
    assert!(after.runs > before.runs);
}

#[test]
fn diffs_of_one_settle_compose_into_one_batch_in_order() {
    let map = CellMap::<&'static str, u32>::new();
    let diffs = Arc::new(Mutex::new(Vec::new()));
    let sink = diffs.clone();
    let _guard = map.subscribe_diffs(move |diff| sink.lock().push(diff.clone()));

    batch(|| {
        map.insert("a", 1);
        map.insert("a", 2);
        map.insert("b", 3);
        map.remove(&"a");
    });

    let diffs = diffs.lock().clone();
    assert_eq!(diffs.len(), 2, "an initial snapshot, then one notification");
    assert_eq!(
        diffs.get(1),
        Some(&MapDiff::Batch {
            changes: vec![
                MapDiff::Insert { key: "a", value: 1 },
                MapDiff::Update {
                    key: "a",
                    old_value: 1,
                    new_value: 2
                },
                MapDiff::Insert { key: "b", value: 3 },
                MapDiff::Remove {
                    key: "a",
                    old_value: 2
                },
            ],
        })
    );
}

#[test]
fn a_join_never_observes_a_multi_node_event_set_torn() {
    // Two "types" that one write always changes together.
    let left = Cell::new(0u64);
    let right = Cell::new(0u64);
    let joined = left.clone().join(right.clone()).materialize();
    let torn = Arc::new(AtomicUsize::new(0));
    let deliveries = Arc::new(AtomicUsize::new(0));
    let (torn_count, delivery_count) = (torn.clone(), deliveries.clone());
    let _guard = joined.subscribe(move |signal| {
        if let Signal::Value(pair) = signal {
            delivery_count.fetch_add(1, Ordering::SeqCst);
            if pair.0 != pair.1 {
                torn_count.fetch_add(1, Ordering::SeqCst);
            }
        }
    });

    thread::scope(|scope| {
        for writer in 0..4u64 {
            let (left, right) = (left.clone(), right.clone());
            scope.spawn(move || {
                for step in 0..500u64 {
                    let value = writer * 1_000 + step;
                    batch_with(&[], Some(Epoch::next()), || {
                        left.set(value);
                        right.set(value);
                    });
                }
            });
        }
    });

    assert_eq!(torn.load(Ordering::SeqCst), 0);
    assert!(deliveries.load(Ordering::SeqCst) > 1);
    assert_eq!(left.get(), right.get());
}

#[test]
fn event_sets_with_one_exclusive_key_settle_separately_and_in_order() {
    let exclusive = Cell::new(0u64);
    let shared = Cell::new(0u64);
    let (exclusive_seen, _exclusive_guard) = record(&exclusive);
    let (shared_seen, _shared_guard) = record(&shared);

    let held = HeldSettle::start();
    // Queued behind the held settle, so the holder pops all six together.
    for value in 1..=3u64 {
        submit(&[], None, || shared.set(value));
    }
    for value in 1..=3u64 {
        submit(&[SettleKey::Exclusive(7)], None, || exclusive.set(value));
    }
    held.release();

    // The same key never shares a settle, so every value is delivered.
    assert_eq!(*exclusive_seen.lock(), vec![1, 2, 3]);
    // Without a key the three sets combine and the last value wins.
    assert_eq!(*shared_seen.lock(), vec![3]);
}

#[test]
fn every_emit_on_a_source_is_delivered_in_order() {
    let source = Source::<u64>::new();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    let _guard = source.subscribe(move |signal| {
        if let Signal::Value(value) = signal {
            sink.lock().push(**value);
        }
    });

    let held = HeldSettle::start();
    for value in 1..=3u64 {
        submit(&[], None, || source.emit(value));
    }
    held.release();
    batch(|| {
        source.emit(4);
        source.emit(5);
    });

    assert_eq!(*seen.lock(), vec![1, 2, 3, 4, 5]);
}

#[test]
fn merge_delivers_every_write_of_both_inputs() {
    // Each input has one writer, and a writer's next write starts after its
    // last one settled, so no two writes of one input share a settle. Writes
    // of the two inputs may share one, and the merge must deliver both.
    let left = Cell::new(0u64);
    let right = Cell::new(0u64);
    let merged = left.clone().merge(right.clone()).materialize();
    let (seen, _guard) = record(&merged);

    thread::scope(|scope| {
        for (input, base) in [(left.clone(), 1_000_000u64), (right.clone(), 2_000_000u64)] {
            scope.spawn(move || {
                for value in 1..=2_000u64 {
                    input.set(base + value);
                }
            });
        }
    });

    let seen = seen.lock().clone();
    assert_eq!(seen.len(), 4_000);
    for base in [1_000_000u64, 2_000_000u64] {
        let from_input: Vec<u64> = seen
            .iter()
            .copied()
            .filter(|value| value / 1_000_000 == base / 1_000_000)
            .collect();
        let expected: Vec<u64> = (1..=2_000u64).map(|value| base + value).collect();
        assert_eq!(from_input, expected, "one input's writes arrive in order");
    }
}

#[test]
fn submit_returns_while_another_thread_holds_the_settle() {
    let cell = Cell::new(0u64);
    let held = HeldSettle::start();

    let started = Instant::now();
    for value in 1..=100u64 {
        submit(&[], None, || cell.set(value));
    }
    let elapsed = started.elapsed();
    assert_eq!(cell.get(), 0, "nothing settles while the settle is held");

    held.release();
    assert!(
        elapsed < Duration::from_secs(5),
        "submit waited on the held settle: {elapsed:?}"
    );
    assert_eq!(cell.get(), 100);
}

#[test]
fn a_map_built_from_entries_is_ready_while_another_thread_holds_the_settle() {
    let held = HeldSettle::start();

    let map = CellMap::from_entries(vec![("a", 1u64), ("b", 2u64)]);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = seen.clone();
    let _guard = map.subscribe_diffs(move |diff| sink.lock().push(diff.clone()));

    assert_eq!(map.get_value(&"a"), Some(1));
    let replayed = seen.lock().clone();
    held.release();
    assert_eq!(map.len().materialize().get(), 2);
    assert!(
        matches!(replayed.as_slice(), [MapDiff::Initial { entries }] if entries.len() == 2),
        "expected one initial snapshot of both entries, got {replayed:?}"
    );
}

#[test]
fn a_waiting_writer_reads_its_own_write_back() {
    let cells: Vec<_> = (0..8).map(|_| Cell::new(0u64)).collect();
    thread::scope(|scope| {
        for cell in &cells {
            scope.spawn(move || {
                let doubled = cell.clone().map(|value| value * 2).materialize();
                for value in 1..=2_000u64 {
                    cell.set(value);
                    assert_eq!(doubled.get(), value * 2);
                }
            });
        }
    });
}

#[test]
fn a_feedback_write_settles_before_the_writer_returns() {
    let input = Cell::new(0u64);
    let feedback = Cell::new(0u64);
    let order = Arc::new(Mutex::new(Vec::new()));

    // `higher` depends on `input`, so the callback's write to it joins the
    // settle in progress. `feedback` is an input, so that write is deferred.
    let higher = input.clone().map(|value| *value).materialize();
    let (log, target) = (order.clone(), feedback.clone());
    let _input_guard = higher.subscribe(move |signal| {
        if let Signal::Value(value) = signal
            && **value > 0
        {
            log.lock().push(format!("input {value}"));
            target.set(**value * 10);
            // Deferred: the callback still reads the old value.
            log.lock().push(format!("read {}", target.get()));
        }
    });
    let log = order.clone();
    let _feedback_guard = feedback.subscribe(move |signal| {
        if let Signal::Value(value) = signal
            && **value > 0
        {
            log.lock().push(format!("feedback {value}"));
        }
    });

    // Contending writers make the test thread a waiting writer some of the
    // time. Either way its write returns with the feedback settled.
    let stop = Arc::new(AtomicBool::new(false));
    thread::scope(|scope| {
        for _ in 0..4 {
            let stop = stop.clone();
            scope.spawn(move || {
                let noise = Cell::new(0u64);
                while !stop.load(Ordering::Relaxed) {
                    noise.set(1);
                }
            });
        }
        let before = hyphae::settle::stats().deferred_writes;
        for value in 1..=200u64 {
            input.set(value);
            assert_eq!(feedback.get(), value * 10);
        }
        assert!(hyphae::settle::stats().deferred_writes >= before + 200);
        stop.store(true, Ordering::Relaxed);
    });

    let order = order.lock().clone();
    assert_eq!(
        order.iter().take(3).cloned().collect::<Vec<_>>(),
        vec!["input 1", "read 0", "feedback 10"]
    );
}

#[test]
fn a_long_feedback_chain_behind_a_waiting_writer_stays_cheap() {
    // Each link writes the next cell twice from its callback, so every
    // generation queues two event sets that wait on the same sleeping writer.
    const LINKS: usize = 28;
    let cells: Vec<_> = (0..=LINKS).map(|_| Cell::new(0u64)).collect();
    let _guards: Vec<_> = cells
        .iter()
        .zip(cells.iter().skip(1))
        .map(|(cell, next)| {
            let next = next.clone();
            cell.subscribe(move |signal| {
                if matches!(signal, Signal::Value(value) if **value == 1) {
                    next.set(1);
                    next.set(1);
                }
            })
        })
        .collect();

    let held = HeldSettle::start();
    let first = cells.first().cloned();
    let writer = thread::spawn(move || first.iter().for_each(|cell| cell.set(1)));
    thread::sleep(Duration::from_millis(200));
    let started = Instant::now();
    held.release();
    assert!(writer.join().is_ok());
    let elapsed = started.elapsed();

    assert_eq!(
        cells.last().map(Gettable::get),
        Some(1),
        "the writer wakes to the settled chain"
    );
    assert!(
        elapsed < Duration::from_secs(1),
        "the chain cost grew with its length: {elapsed:?}"
    );
}

#[test]
fn writing_under_a_lock_a_subscriber_needs_does_not_deadlock() {
    // The hazard: a writer holds `lock` across a write, and a subscriber that
    // another thread is running needs `lock`. A waiting write would deadlock.
    // The safe form queues the write with `enqueue`, which runs no callback
    // on this thread, releases the lock, and then waits with `flush`.
    let lock = Arc::new(Mutex::new(0u64));
    let cell = Cell::new(0u64);
    let subscriber_lock = lock.clone();
    let _guard = cell.subscribe(move |signal| {
        if let Signal::Value(value) = signal {
            *subscriber_lock.lock() = **value;
        }
    });

    let (done_tx, done_rx) = mpsc::channel();
    let workers: Vec<_> = (0..4u64)
        .map(|worker| {
            let (lock, cell, done) = (lock.clone(), cell.clone(), done_tx.clone());
            thread::spawn(move || {
                for step in 0..500u64 {
                    {
                        let _held = lock.lock();
                        enqueue(&[], None, || cell.set(worker * 1_000 + step));
                    }
                    flush();
                }
                let _ = done.send(());
            })
        })
        .collect();
    for _ in 0..4 {
        assert!(
            done_rx.recv_timeout(Duration::from_mins(1)).is_ok(),
            "a writer deadlocked against a subscriber"
        );
    }
    for worker in workers {
        assert!(worker.join().is_ok());
    }
    let settled = *lock.lock();
    assert_eq!(settled, cell.get());
}

#[test]
fn a_row_update_from_a_per_row_cell_joins_the_settle_that_fired_the_cell() {
    let rows = CellMap::<String, i64>::new();
    let weights = CellMap::<String, i64>::new();
    let offset = Cell::new(0i64);
    rows.insert("a".to_string(), 10);
    weights.insert("a".to_string(), 2);

    let per_row = weights.clone();
    let projected = rows
        .project_cell(move |key, value| {
            let (key, value) = (key.clone(), *value);
            per_row
                .get(&key)
                .map(move |weight| weight.map(|weight| (key.clone(), value * weight)))
                .materialize()
        })
        .materialize();
    let joined = projected
        .get(&"a".to_string())
        .materialize()
        .join(offset.clone())
        .materialize();
    let (seen, _guard) = record(&joined);

    batch(|| {
        weights.insert("a".to_string(), 5);
        offset.set(1);
    });

    // One delivery, never the new offset beside the row's old value.
    assert_eq!(*seen.lock(), vec![(Some(50), 1)]);
}

#[test]
fn a_map_built_inside_a_settle_keeps_a_row_update_from_that_settle() {
    let trigger = Cell::new(false);
    let weight = Cell::new(2i64);
    // Two operators above `weight`, so the row cell fires after the callback
    // of `trigger` has built the map from the row's old value.
    let row_weight = weight.clone().map(|weight| *weight).materialize();
    let row_weight = row_weight.map(|weight| *weight).materialize();

    let built = Arc::new(Mutex::new(None));
    let slot = built.clone();
    let _guard = trigger.subscribe(move |signal| {
        if !matches!(signal, Signal::Value(value) if **value) {
            return;
        }
        let row_weight = row_weight.clone();
        let projected = CellMap::<String, i64>::from_entries(vec![("a".to_string(), 10i64)])
            .project_cell(move |key, value| {
                let (key, value) = (key.clone(), *value);
                row_weight
                    .clone()
                    .map(move |weight| Some((key.clone(), value * weight)))
                    .materialize()
            })
            .materialize();
        let entries = projected.entries().materialize();
        *slot.lock() = Some((projected, entries));
    });

    batch(|| {
        weight.set(5);
        trigger.set(true);
    });

    let observed = built
        .lock()
        .take()
        .map(|(projected, entries)| (projected.get_value(&"a".to_string()), entries.get()));
    // The stored row, then what a subscriber that replays the map's diffs
    // ends with.
    assert_eq!(observed, Some((Some(50), vec![("a".to_string(), 50)])));
}

#[test]
fn a_write_that_could_join_stays_behind_an_earlier_queued_write_to_its_cell() {
    let trigger = Cell::new(0u64);
    let other = Cell::new(0u64);
    let target = Cell::new(0u64);
    let (seen, _seen_guard) = record(&target);
    let written = target.clone();
    let _guard = trigger.subscribe(move |signal| {
        if !matches!(signal, Signal::Value(value) if **value == 1) {
            return;
        }
        // An input cell, so this write waits for a later settle.
        written.set(1);
        // The cell gains a dependency and rises above the firing node, so
        // by height alone the next write would join this settle.
        written.own(other.subscribe(|_| {}));
        written.set(2);
    });

    trigger.set(1);

    assert_eq!(target.get(), 2);
    let seen = seen.lock().clone();
    assert_eq!(seen.last(), Some(&2), "seen = {seen:?}");
}
