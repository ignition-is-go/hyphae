//! `no_coalesce` opt-out for the propagation scheduler (`scheduler` feature).
//!
//! The scheduler's default is last-write-wins coalescing per cell under
//! [`batch`] — correct for behavior cells, but it silently drops intermediate
//! emissions that an *event*-semantic consumer needs. These tests pin the
//! escape hatch: a cell marked [`Cell::no_coalesce`] (or born inside a
//! [`no_coalesce`](hyphae::scheduler::no_coalesce) scope) enqueues every notify
//! as a distinct, arrival-ordered op, so nothing is dropped — while an
//! untagged cell alongside it still coalesces.
//!
//! The accumulator sink is deliberately hand-rolled (event state in a
//! subscriber closure) because that is exactly rship's shape: the place a
//! dropped intermediate corrupts a fold.
#![cfg(feature = "scheduler")]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use hyphae::scheduler::no_coalesce;
use hyphae::{batch, Cell, CellMutable, Mutable, Signal, Watchable};

/// A hand-rolled accumulator: sum every value the cell emits. Returns the live
/// accumulator slot. Reset it to 0 after wiring to discard the subscribe-time
/// seed replay.
fn sum_sink(cell: &Cell<i64, CellMutable>) -> Arc<Mutex<i64>> {
    let acc = Arc::new(Mutex::new(0i64));
    let sink = acc.clone();
    let guard = cell.subscribe(move |sig| {
        if let Signal::Value(v) = sig {
            *sink.lock().unwrap() += **v;
        }
    });
    std::mem::forget(guard);
    acc
}

/// Count every value emitted by `cell` (fanouts observed by a subscriber).
fn fire_counter<M: Send + Sync + 'static>(cell: &Cell<i64, M>) -> Arc<AtomicUsize> {
    let fires = Arc::new(AtomicUsize::new(0));
    let f = fires.clone();
    let guard = cell.subscribe(move |sig| {
        if let Signal::Value(_) = sig {
            f.fetch_add(1, Ordering::SeqCst);
        }
    });
    std::mem::forget(guard);
    fires
}

#[test]
fn coalescing_source_drops_an_intermediate_set_in_a_batch() {
    // Default (coalescing): two sets in one batch collapse last-write-wins, so
    // the accumulator never sees the intermediate `1` — it settles once at `2`.
    let s = Cell::new(0i64);
    let acc = sum_sink(&s);
    *acc.lock().unwrap() = 0; // discard seed replay
    batch(|| {
        s.set(1);
        s.set(2);
    });
    assert_eq!(
        *acc.lock().unwrap(),
        2,
        "a coalescing source drops the intermediate emission under batch"
    );
}

#[test]
fn no_coalesce_source_preserves_every_set_in_a_batch() {
    // Marked no_coalesce: both sets survive as distinct height-ordered ops, so
    // the accumulator folds every emission — 0 + 1 + 2.
    let s = Cell::new(0i64).no_coalesce();
    let acc = sum_sink(&s);
    *acc.lock().unwrap() = 0;
    batch(|| {
        s.set(1);
        s.set(2);
    });
    assert_eq!(
        *acc.lock().unwrap(),
        1 + 2,
        "no_coalesce preserves every emission under batch"
    );
}

#[test]
fn no_coalesce_scope_stamps_cells_born_inside() {
    // A cell constructed inside the scope is stamped at birth...
    let inside = no_coalesce(|| Cell::new(0i64));
    let acc_in = sum_sink(&inside);
    *acc_in.lock().unwrap() = 0;
    batch(|| {
        inside.set(1);
        inside.set(2);
    });
    assert_eq!(*acc_in.lock().unwrap(), 3, "cell born in scope is no_coalesce");

    // ...while a cell born outside it coalesces as usual.
    let outside = Cell::new(0i64);
    let acc_out = sum_sink(&outside);
    *acc_out.lock().unwrap() = 0;
    batch(|| {
        outside.set(1);
        outside.set(2);
    });
    assert_eq!(
        *acc_out.lock().unwrap(),
        2,
        "cell born outside the scope still coalesces"
    );
}

#[test]
fn no_coalesce_is_inert_outside_a_batch() {
    // Synchronous propagation already sees every emission; the tag changes
    // nothing off the batch path.
    let tagged = Cell::new(0i64).no_coalesce();
    let acc = sum_sink(&tagged);
    *acc.lock().unwrap() = 0;
    tagged.set(1);
    tagged.set(2);
    assert_eq!(
        *acc.lock().unwrap(),
        3,
        "synchronous path is unchanged by the tag"
    );
}

#[test]
fn no_coalesce_scope_preserves_multiplicity_but_settles_glitch_free() {
    // A no_coalesce cell fed by a diamond re-fires per input arrival (multiplicity
    // preserved), but each fire reads *settled* inputs — it is deferred and
    // height-ordered like everything else; only the last-write-wins drop is
    // skipped. So the final settled value is correct, never a stale glitch.
    //
    // Crucially, `join` *materializes* an intermediate cell, so the whole
    // diamond must be built inside a `no_coalesce` scope — tagging only the
    // final sink would leave that intermediate join cell coalescing, and it
    // would collapse the two arrivals before the sink ever saw them. This is
    // exactly the "every materialized cell on the path" rule.
    use hyphae::{JoinExt, MapExt, MaterializeDefinite};

    let s = Cell::new(0i64);
    let sink = no_coalesce(|| {
        let a = s.clone().map(|x| x + 1).materialize();
        let b = s.clone().map(|x| x * 10).materialize();
        a.join(&b).map(|(x, y)| x + y).materialize()
    });

    let fires = fire_counter(&sink);
    let last = {
        let slot = Arc::new(Mutex::new(0i64));
        let sink_slot = slot.clone();
        let g = sink.subscribe(move |sig| {
            if let Signal::Value(v) = sig {
                *sink_slot.lock().unwrap() = **v;
            }
        });
        std::mem::forget(g);
        slot
    };

    fires.store(0, Ordering::SeqCst);
    batch(|| s.set(5));

    // Two arrivals (one per diamond leg) survive coalescing...
    assert!(
        fires.load(Ordering::SeqCst) >= 2,
        "no_coalesce sink keeps both diamond arrivals, got {}",
        fires.load(Ordering::SeqCst)
    );
    // ...and the settled value is the height-ordered result: (5+1) + (5*10).
    assert_eq!(
        *last.lock().unwrap(),
        (5 + 1) + (5 * 10),
        "final value is glitch-free despite preserved multiplicity"
    );
}

#[test]
fn batched_source_coalesces_a_same_rate_diamond_that_per_subscriber_batch_cannot() {
    use hyphae::{JoinExt, MapExt, MaterializeDefinite, Source};

    // rship's shape: two cells sample the SAME source, each via its OWN
    // subscriber callback that opens its OWN batch around the set. A node
    // joining the two same-rate values then re-fires once per subscriber,
    // because the two arrivals land in two separate propagation passes —
    // per-call-site batching cannot coalesce across subscribers.
    fn wire(src: &Source<u64>) -> (Cell<i64, hyphae::CellImmutable>, std::sync::Arc<AtomicUsize>) {
        let a = Cell::new(0i64);
        let b = Cell::new(0i64);
        let (a2, b2) = (a.clone(), b.clone());
        std::mem::forget(src.subscribe(move |s| {
            if let Signal::Value(v) = s {
                batch(|| a2.set(**v as i64));
            }
        }));
        std::mem::forget(src.subscribe(move |s| {
            if let Signal::Value(v) = s {
                batch(|| b2.set(**v as i64 * 10));
            }
        }));
        // k is fed by both legs; it owns the subscriptions to a and b, keeping
        // them alive after the locals drop.
        let k = a.join(&b).map(|(x, y)| x + y).materialize();
        let fires = fire_counter(&k);
        (k, fires)
    }

    // Unbatched source → two separate batch windows → the join fires twice.
    let plain = Source::new();
    let (k1, f1) = wire(&plain);
    f1.store(0, Ordering::SeqCst);
    plain.emit(5);
    assert_eq!(
        f1.load(Ordering::SeqCst),
        2,
        "per-subscriber batch can't coalesce arrivals from different subscribers"
    );
    let _ = k1;

    // Batched source → the whole fan-out is one pass; the per-subscriber batches
    // nest into it, so the join settles once for the emit.
    let batched = Source::new().batched();
    let (k2, f2) = wire(&batched);
    f2.store(0, Ordering::SeqCst);
    batched.emit(5);
    assert_eq!(
        f2.load(Ordering::SeqCst),
        1,
        "batched fan-out coalesces the same-rate diamond to a single fire"
    );
    let _ = k2;
}

#[test]
fn cellmap_diffs_survive_a_batched_add_then_remove() {
    use hyphae::CellMap;

    let map: CellMap<u32, u32> = CellMap::new();
    let seen = std::sync::Arc::new(Mutex::new(0usize));
    let sink = seen.clone();
    let guard = map.subscribe_diffs(move |_diff| {
        *sink.lock().unwrap() += 1;
    });
    std::mem::forget(guard);
    *seen.lock().unwrap() = 0; // discard the Initial replay

    // Add then remove the same key inside one batch: two diffs_cell.set calls.
    // The diffs_cell is no_coalesce by default, so BOTH the Insert and Remove
    // survive; a plain (coalescing) cell would drop the Insert (LWW keeps the
    // Remove) and an accumulating subscriber would corrupt.
    batch(|| {
        map.insert(1, 100);
        map.remove(&1);
    });

    assert!(
        *seen.lock().unwrap() >= 2,
        "both the add and the remove diff must survive the batch, saw {}",
        *seen.lock().unwrap()
    );
}
