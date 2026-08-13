//! Phase 0 gate for the opt-in propagation scheduler (`scheduler` feature).
//!
//! Two properties, both asserted against the public API:
//!   1. A synchronous diamond re-solves its sink **twice** per source change
//!      (documents the glitch), while the same change inside `batch` solves it
//!      **once** — coalesced, with the correct settled value.
//!   2. Height ordering: on an unequal-length diamond, the join settles with
//!      the value from the *long* path, not a stale short-path glitch.
//!
//! The whole file compiles to nothing unless the feature is on.
#![cfg(feature = "scheduler")]

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use hyphae::{
    Cell, FilterExt, JoinExt, MapExt, Materialize, Mutable, Signal, SwitchMapExt, Watchable, batch,
};

static SCHEDULER_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The scheduler's tick queue is a single process-wide structure (see
/// `hyphae::scheduler`'s cross-thread docs) — correct for one real server
/// process, but it means `cargo test`'s default of running every `#[test]`
/// fn in this binary as a concurrent thread would let these tests interfere
/// with each other's batches. Serialize them.
fn scheduler_test_serial() -> std::sync::MutexGuard<'static, ()> {
    // Force the wave-parallel drain path at test width: production defaults
    // the group threshold high (waves stay sequential at rest), so parallelism
    // tests must lower it to actually exercise concurrent same-height groups.
    hyphae::scheduler::set_wave_threshold_for_test(4);
    SCHEDULER_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Subscribe to `cell`, counting Value emits and recording the last one.
fn count_and_record<M: Send + Sync + 'static>(
    cell: &Cell<i64, M>,
) -> (Arc<AtomicUsize>, Arc<Mutex<i64>>) {
    let fires = Arc::new(AtomicUsize::new(0));
    let last = Arc::new(Mutex::new(0i64));
    let (f, l) = (fires.clone(), last.clone());
    let guard = cell.subscribe(move |sig| {
        if let Signal::Value(v) = sig {
            f.fetch_add(1, Ordering::SeqCst);
            *l.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = **v;
        }
    });
    std::mem::forget(guard);
    (fires, last)
}

/// Record the last value emitted by `cell` into a shared slot.
fn record_last<M: Send + Sync + 'static>(cell: &Cell<i64, M>) -> Arc<Mutex<i64>> {
    let slot = Arc::new(Mutex::new(0i64));
    let sink = slot.clone();
    let guard = cell.subscribe(move |sig| {
        if let Signal::Value(v) = sig {
            *sink
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = **v;
        }
    });
    // Keep the subscription alive for the lifetime of the slot.
    std::mem::forget(guard);
    slot
}

#[test]
fn diamond_synchronous_solves_twice_batched_once() {
    let _serial = scheduler_test_serial();
    // s ─┬─> a = s + 1 ─┐
    //    └─> b = s * 10 ┴─> j = (a, b) ─> k = a + b  (the "solve")
    let source = Cell::new(0i64);
    let branch_a = source
        .clone()
        .map(|value| value.saturating_add(1))
        .materialize();
    let branch_b = source
        .clone()
        .map(|value| value.saturating_mul(10))
        .materialize();
    let joined = branch_a.join(branch_b);

    let solve_count = Arc::new(AtomicUsize::new(0));
    let counter = solve_count.clone();
    let result = joined
        .map(move |(left, right)| {
            counter.fetch_add(1, Ordering::SeqCst);
            left.saturating_add(*right)
        })
        .materialize();
    let last = record_last(&result);

    // Synchronous: the shared source reaches the sink via both diamond legs,
    // so the sink re-solves twice for one source change.
    solve_count.store(0, Ordering::SeqCst);
    source.set(5);
    assert_eq!(
        solve_count.load(Ordering::SeqCst),
        2,
        "synchronous diamond re-solves once per leg"
    );
    assert_eq!(
        *last
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        56
    );

    // Batched: coalesced to a single settled solve, same final value.
    solve_count.store(0, Ordering::SeqCst);
    batch(|| source.set(9));
    assert_eq!(
        solve_count.load(Ordering::SeqCst),
        1,
        "batched diamond solves once"
    );
    assert_eq!(
        *last
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        100
    );
}

#[test]
fn unequal_diamond_settles_in_height_order() {
    let _serial = scheduler_test_serial();
    // Long leg:  s ─> a = s + 1 ─> c = a * 2      (height 2)
    // Short leg: s ─────────────────────────────  (height 0)
    //            j = (c, s)   height 3 ; without height ordering the short leg
    //            would fire j early with a stale c.
    let source = Cell::new(0i64);
    let first_leg = source
        .clone()
        .map(|value| value.saturating_add(1))
        .materialize();
    let long_leg = first_leg.map(|value| value.saturating_mul(2)).materialize();
    let joined = long_leg.join(source.clone());

    let solves = Arc::new(AtomicUsize::new(0));
    let counter = solves.clone();
    // Encode both inputs into one i64 so a stale `c` would be observable:
    // value = c * 1000 + s.
    let encoded = joined
        .map(move |(long_value, source_value)| {
            counter.fetch_add(1, Ordering::SeqCst);
            long_value
                .saturating_mul(1000)
                .saturating_add(*source_value)
        })
        .materialize();
    let last = record_last(&encoded);

    solves.store(0, Ordering::SeqCst);
    batch(|| source.set(7));

    // c settles to (7 + 1) * 2 = 16 before j pops, so the sink sees (16, 7).
    assert_eq!(
        solves.load(Ordering::SeqCst),
        1,
        "unequal diamond still solves once under batch"
    );
    assert_eq!(
        *last
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        16_007,
        "join settled with the long-leg value, not a stale short-leg glitch"
    );
}

#[test]
fn diamond_with_unequal_branch_heights_solves_once() {
    let _serial = scheduler_test_serial();
    // short: s ─> a                     (a at height 1)
    // long:  s ─> b ─> c ─> d           (d at height 3)
    // j = (a, d)                        (height 4)
    //
    // `a` reaches j three levels early. It must NOT fire j before the long
    // branch settles `d` — j is enqueued at its *own* height (4), not at a's,
    // so the drain still puts every branch below j regardless of their heights.
    let source = Cell::new(0i64);
    let short_leg = source
        .clone()
        .map(|value| value.saturating_add(1))
        .materialize(); // h1
    let long_leg_one = source
        .clone()
        .map(|value| value.saturating_add(100))
        .materialize(); // h1
    let long_leg_two = long_leg_one
        .map(|value| value.saturating_add(1000))
        .materialize(); // h2
    let long_leg_three = long_leg_two
        .map(|value| value.saturating_add(10000))
        .materialize(); // h3
    let joined = short_leg.join(long_leg_three); // h4

    let solves = Arc::new(AtomicUsize::new(0));
    let counter = solves.clone();
    let encoded = joined
        .map(move |(short_value, long_value)| {
            counter.fetch_add(1, Ordering::SeqCst);
            short_value
                .saturating_mul(1_000_000)
                .saturating_add(*long_value)
        })
        .materialize();
    let last = record_last(&encoded);

    solves.store(0, Ordering::SeqCst);
    batch(|| source.set(7));
    assert_eq!(
        solves.load(Ordering::SeqCst),
        1,
        "unequal-height diamond still solves once"
    );
    // a = 8 ; d = 7 + 100 + 1000 + 10000 = 11107. A correct d proves j waited
    // for the tall branch even though a arrived first.
    assert_eq!(
        *last
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        8_011_107
    );
}

#[test]
fn filtered_diamond_does_not_wait_for_a_suppressed_branch() {
    let _serial = scheduler_test_serial();
    // s ─> a = s + 1                 (always emits)
    //   └─> b = (s if even) * 10     (branch suppressed when s is odd)
    // j = (a, b) ─> k = a + b
    //
    // The failure this guards against: a barrier that waits for "both inputs"
    // would block forever on odd `s`, because the filtered branch never
    // notifies. Height ordering doesn't count arrivals, so it can't.
    let source = Cell::new(0i64); // even seed, so b materializes with a value
    let always_branch = source
        .clone()
        .map(|value| value.saturating_add(1))
        .materialize();
    let filtered_branch = source
        .clone()
        .filter(|value| value.rem_euclid(2) == 0)
        .map(|value| value.saturating_mul(10))
        .materialize();
    let joined = always_branch.join(filtered_branch);

    let solve_count = Arc::new(AtomicUsize::new(0));
    let counter = solve_count.clone();
    let result = joined
        .map(move |(always, filtered)| {
            counter.fetch_add(1, Ordering::SeqCst);
            always.saturating_add((*filtered).unwrap_or(0))
        })
        .materialize();
    let last = record_last(&result);

    // Odd source: b is filtered out — only a arrives. The batch must settle the
    // join from the single live branch and return, not hang waiting for b.
    solve_count.store(0, Ordering::SeqCst);
    batch(|| source.set(3));
    assert_eq!(
        solve_count.load(Ordering::SeqCst),
        1,
        "join settles from the one live branch"
    );
    assert_eq!(
        *last
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        4,
        "a updated, b held at 0"
    );

    // Even source: both branches emit — synchronous would solve twice, batch
    // coalesces to a single settled solve.
    solve_count.store(0, Ordering::SeqCst);
    batch(|| source.set(4));
    assert_eq!(
        solve_count.load(Ordering::SeqCst),
        1,
        "both branches live, still one settled solve"
    );
    assert_eq!(
        *last
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        45
    );
}

#[test]
fn switch_map_rewire_and_taller_inner_update_in_one_batch() {
    let _serial = scheduler_test_serial();
    // Inner 0 is a bare source (height 0); inner 1 is two maps deep (height 2).
    // In one batch we switch to inner 1 AND drive its source. The result must
    // end at inner 1's *settled* value, even though the rewire bumps the
    // topology epoch and lifts the result's own height mid-drain.
    let in0 = Cell::new(0i64).map(|x| *x).materialize(); // immutable inner, h1
    let in1_src = Cell::new(0i64);
    let in1 = in1_src
        .clone()
        .map(|value| value.saturating_add(1))
        .map(|value| value.saturating_mul(2))
        .materialize(); // h2

    let sel = Cell::new(0i64);
    let (in0c, in1c) = (in0, in1);
    let result = sel
        .clone()
        .switch_map(move |&i| if i == 0 { in0c.clone() } else { in1c.clone() })
        .materialize();
    let (_fires, last) = count_and_record(&result);

    batch(|| {
        sel.set(1);
        in1_src.set(5);
    });
    assert_eq!(
        *last
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        12,
        "settled to inner 1's value"
    );
}

#[test]
fn switch_map_old_inner_firing_during_switch_is_glitch_free() {
    let _serial = scheduler_test_serial();
    // The hazard: in one batch the OLD inner fires *and* we switch to a taller
    // new inner. Naively the result is enqueued at the old (short) height, fires
    // early with the stale old value, then fires again at the new height — a
    // double fire with an observable wrong intermediate. Run many trials because
    // the drain's same-height tie-break is id-ordered (nondeterministic), so the
    // bad interleaving only occurs on some runs.
    for trial in 0..200 {
        let in0_src = Cell::new(0i64);
        let in0 = in0_src.clone().map(|x| *x).materialize(); // immutable inner, h1
        let in1_src = Cell::new(0i64);
        let in1 = in1_src
            .clone()
            .map(|value| value.saturating_add(1))
            .map(|value| value.saturating_mul(2))
            .map(|value| value.saturating_add(100))
            .materialize(); // taller inner, h3

        let sel = Cell::new(0i64);
        let (in0c, in1c) = (in0.clone(), in1.clone());
        let result = sel
            .clone()
            .switch_map(move |&i| if i == 0 { in0c.clone() } else { in1c.clone() })
            .materialize();
        let (fires, last) = count_and_record(&result);

        fires.store(0, Ordering::SeqCst);
        batch(|| {
            in0_src.set(7); // old inner fires
            sel.set(1); // switch away to the taller inner
            in1_src.set(5); // new inner settles to (5+1)*2 + 100 = 112
        });

        assert_eq!(
            *last
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            112,
            "trial {trial}: result settled to the switched-in inner"
        );
        assert_eq!(
            fires.load(Ordering::SeqCst),
            1,
            "trial {trial}: exactly one settled fire, no stale old-inner glitch"
        );
    }
}

#[test]
fn batch_returns_closure_value_and_nests() {
    let _serial = scheduler_test_serial();
    let s = Cell::new(0i64);
    let doubled = s.clone().map(|value| value.saturating_mul(2)).materialize();
    let last = record_last(&doubled);

    // `batch` returns the closure's value.
    let out = batch(|| {
        s.set(4);
        // Nested batch joins the outer tick; only the outermost drains.
        batch(|| s.set(21));
        99
    });
    assert_eq!(out, 99);
    // Last write wins within the tick: doubled settles from s = 21.
    assert_eq!(
        *last
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        42
    );
}
