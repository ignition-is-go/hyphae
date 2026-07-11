//! Wave-parallel regression harness for hyphae's single-input transform
//! operators.
//!
//! hyphae's opt-in `scheduler` feature defers `batch(|| ...)` notifies into a
//! height-ordered queue and, once a height's wave reaches
//! `PARALLEL_WAVE_THRESHOLD` (8) ops, dispatches that whole wave across rayon
//! IN PARALLEL. This file pins down two properties of the single-input
//! transform operators under that parallel wave:
//!
//! ## Template B — wide independent wave (expected PASS)
//!
//! A single-input operator has exactly one upstream subscription, so it can't
//! suffer the two-sibling "torn value" race a `join` can. What still has to
//! hold is that many *independent* instances of the operator, all settling in
//! ONE parallel wave, each settle to exactly the value their own input
//! dictates — no cross-instance corruption when rayon runs them concurrently.
//! Each Template-B test builds 16 (> 8) independent source→operator→cell
//! graphs, drives all 16 sources in one `batch()` (16 height-0 source ops →
//! one parallel wave; then 16 height-1 derived ops → another), and asserts
//! every derived cell holds its exact, per-instance-distinct value. Graphs are
//! rebuilt each iteration so every operator's internal state (scan's
//! accumulator, distinct's seen-set, ...) starts from a known seed and the
//! expected value stays exactly computable.
//!
//! ## Template C — event order under `no_coalesce` (expected FAIL)
//!
//! A CONFIRMED scheduler bug: a `no_coalesce` (event) cell fired >= 8 times in
//! ONE batch lands all its ops in a single height wave; because that wave is
//! dispatched across rayon, the ops execute — and their fanouts deliver — in
//! nondeterministic order, so the events are REORDERED. Any event-semantic
//! operator (`scan`, `distinct_until_changed_by`, `deduped`, `state_transition`)
//! built on a `no_coalesce` source is exposed: its stateful closure sees the
//! events out of order. To observe each emission (rather than a single
//! last-write-wins-coalesced settle) the whole source→operator→cell graph is
//! constructed inside `no_coalesce`, so the derived cell's per-event notifies
//! also survive; a subscriber on it records the delivered sequence. Each
//! Template-C test fires 32 in-order events per batch and asserts the derived
//! cell delivered them 1,2,...,32 in order and never ran its subscriber
//! concurrently with itself — assertions the reorder bug currently breaks.
//! These are the regression sentinels: they go green when the parallel wave is
//! made event-order-safe for `no_coalesce` subgraphs.
#![cfg(feature = "scheduler")]
#![allow(clippy::needless_range_loop)] // deliberate parallel-vec indexing

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicI64, AtomicUsize, Ordering},
};

use hyphae::{
    Cell, CellImmutable, CellMutable, DedupedExt, DistinctExt, DistinctUntilChangedByExt,
    FilterExt, Gettable, MapErrExt, MapExt, MapOkExt, MaterializeDefinite, MaterializeEmpty,
    Mutable, ScanExt, Signal, StateTransitionExt, TapExt, TryMapExt, Watchable, batch,
    scheduler::no_coalesce,
};

/// The scheduler's tick queue is a single process-wide structure, so tests
/// that exercise `batch()` would interfere when `cargo test` runs this file's
/// `#[test]` fns as concurrent threads. Serialize them. (Copied from
/// `scheduler_completeness.rs`.)
fn scheduler_test_serial() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Template-B wave width: 16 > PARALLEL_WAVE_THRESHOLD (8), so every batch
/// forces a genuinely parallel wave rather than the small-wave sequential
/// fallback.
const N_B: usize = 16;
/// Template-B iteration count — enough to shake out wave nondeterminism.
const ITERS_B: i64 = 2_000;

/// Template-C burst width: 32 no_coalesce events per batch, well over the
/// parallel-wave threshold.
const N_C: i64 = 32;
/// Template-C iteration count.
const ITERS_C: usize = 2_000;

// ---------------------------------------------------------------------------
// Template B — wide independent wave, exact per-instance settle (expect PASS)
// ---------------------------------------------------------------------------

#[test]
fn map_wide_wave_settles_each_instance_correctly() {
    let _serial = scheduler_test_serial();
    for iter in 0..ITERS_B {
        let base = iter * 1_000;
        let mut sources = Vec::with_capacity(N_B);
        let mut derived = Vec::with_capacity(N_B);
        for k in 0..N_B {
            let kk = k as i64;
            let s = Cell::new(0i64);
            let d = s.clone().map(move |x| x * (kk + 1) + 7).materialize();
            sources.push(s);
            derived.push(d);
        }
        batch(|| {
            for (k, s) in sources.iter().enumerate() {
                s.set(base + k as i64 + 1);
            }
        });
        for k in 0..N_B {
            let x = base + k as i64 + 1;
            assert_eq!(
                derived[k].get(),
                x * (k as i64 + 1) + 7,
                "map instance {k} settled wrong at iter {iter}"
            );
        }
    }
}

#[test]
fn filter_wide_wave_settles_each_instance_correctly() {
    let _serial = scheduler_test_serial();
    // Even inputs pass the predicate (cell -> Some(v)); odd inputs are
    // swallowed and the cell stays at its seed Some(0). Per-instance both
    // branches are exercised in one wave.
    for iter in 0..ITERS_B {
        let base = iter * 1_000; // even
        let mut sources = Vec::with_capacity(N_B);
        let mut derived = Vec::with_capacity(N_B);
        for _ in 0..N_B {
            let s = Cell::new(0i64);
            let d = s.clone().filter(|x| x % 2 == 0).materialize();
            sources.push(s);
            derived.push(d);
        }
        batch(|| {
            for (k, s) in sources.iter().enumerate() {
                s.set(base + k as i64 + 1);
            }
        });
        for k in 0..N_B {
            let v = base + k as i64 + 1;
            let expected = if v % 2 == 0 { Some(v) } else { Some(0) };
            assert_eq!(
                derived[k].get(),
                expected,
                "filter instance {k} settled wrong at iter {iter}"
            );
        }
    }
}

#[test]
fn try_map_wide_wave_settles_each_instance_correctly() {
    let _serial = scheduler_test_serial();
    // Even inputs -> Ok(x*2); odd inputs -> Err. Both variants settle in one
    // wave, one per independent instance.
    for iter in 0..ITERS_B {
        let base = iter * 1_000;
        let mut sources = Vec::with_capacity(N_B);
        let mut derived = Vec::with_capacity(N_B);
        for _ in 0..N_B {
            let s = Cell::new(0i64);
            let d = s
                .clone()
                .try_map(|x| {
                    if x % 2 == 0 {
                        Ok(x * 2)
                    } else {
                        Err(format!("odd:{x}"))
                    }
                })
                .materialize();
            sources.push(s);
            derived.push(d);
        }
        batch(|| {
            for (k, s) in sources.iter().enumerate() {
                s.set(base + k as i64 + 1);
            }
        });
        for k in 0..N_B {
            let v = base + k as i64 + 1;
            let expected: Result<i64, String> = if v % 2 == 0 {
                Ok(v * 2)
            } else {
                Err(format!("odd:{v}"))
            };
            assert_eq!(
                derived[k].get(),
                expected,
                "try_map instance {k} settled wrong at iter {iter}"
            );
        }
    }
}

#[test]
fn tap_wide_wave_settles_each_instance_correctly() {
    let _serial = scheduler_test_serial();
    // tap forwards the value unchanged and runs a side effect; both the
    // passed-through cell value AND the recorded side effect must reflect this
    // instance's own input, not a neighbour's.
    for iter in 0..ITERS_B {
        let base = iter * 1_000;
        let mut sources = Vec::with_capacity(N_B);
        let mut derived = Vec::with_capacity(N_B);
        let side: Vec<Arc<AtomicI64>> = (0..N_B).map(|_| Arc::new(AtomicI64::new(-1))).collect();
        for k in 0..N_B {
            let s = Cell::new(0i64);
            let sc = side[k].clone();
            let d = s
                .clone()
                .tap(move |v| sc.store(*v, Ordering::SeqCst))
                .materialize();
            sources.push(s);
            derived.push(d);
        }
        batch(|| {
            for (k, s) in sources.iter().enumerate() {
                s.set(base + k as i64 + 1);
            }
        });
        for k in 0..N_B {
            let v = base + k as i64 + 1;
            assert_eq!(derived[k].get(), v, "tap value {k} wrong at iter {iter}");
            assert_eq!(
                side[k].load(Ordering::SeqCst),
                v,
                "tap side effect {k} wrong at iter {iter}"
            );
        }
    }
}

#[test]
fn scan_wide_wave_settles_each_instance_correctly() {
    let _serial = scheduler_test_serial();
    // Fresh graph each iteration so the accumulator starts at 100 every time.
    // One event advances it to 100 + v; each instance's v is distinct.
    for iter in 0..ITERS_B {
        let base = iter * 1_000;
        let mut sources = Vec::with_capacity(N_B);
        let mut derived = Vec::with_capacity(N_B);
        for _ in 0..N_B {
            let s = Cell::new(0i64);
            let d = s.clone().scan(100i64, |acc, x| acc + x).materialize();
            sources.push(s);
            derived.push(d);
        }
        batch(|| {
            for (k, s) in sources.iter().enumerate() {
                s.set(base + k as i64 + 1);
            }
        });
        for k in 0..N_B {
            let v = base + k as i64 + 1;
            assert_eq!(
                derived[k].get(),
                100 + v,
                "scan instance {k} settled wrong at iter {iter}"
            );
        }
    }
}

#[test]
fn distinct_wide_wave_settles_each_instance_correctly() {
    let _serial = scheduler_test_serial();
    // Inputs are strictly increasing and never repeat the seed 0, so every set
    // is a genuinely new value that passes `distinct` and updates the cell.
    for iter in 0..ITERS_B {
        let base = iter * 1_000;
        let mut sources = Vec::with_capacity(N_B);
        let mut derived = Vec::with_capacity(N_B);
        for _ in 0..N_B {
            let s = Cell::new(0i64);
            let d = s.clone().distinct().materialize();
            sources.push(s);
            derived.push(d);
        }
        batch(|| {
            for (k, s) in sources.iter().enumerate() {
                s.set(base + k as i64 + 1);
            }
        });
        for k in 0..N_B {
            let v = base + k as i64 + 1;
            assert_eq!(
                derived[k].get(),
                v,
                "distinct instance {k} settled wrong at iter {iter}"
            );
        }
    }
}

#[test]
fn distinct_until_changed_by_wide_wave_settles_each_instance_correctly() {
    let _serial = scheduler_test_serial();
    // Each input differs from the seed 0, so the comparator (==) reports
    // "changed" and the cell updates to the new, per-instance-distinct value.
    for iter in 0..ITERS_B {
        let base = iter * 1_000;
        let mut sources = Vec::with_capacity(N_B);
        let mut derived = Vec::with_capacity(N_B);
        for _ in 0..N_B {
            let s = Cell::new(0i64);
            let d = s
                .clone()
                .distinct_until_changed_by(|a, b| a == b)
                .materialize();
            sources.push(s);
            derived.push(d);
        }
        batch(|| {
            for (k, s) in sources.iter().enumerate() {
                s.set(base + k as i64 + 1);
            }
        });
        for k in 0..N_B {
            let v = base + k as i64 + 1;
            assert_eq!(
                derived[k].get(),
                v,
                "distinct_until_changed_by instance {k} settled wrong at iter {iter}"
            );
        }
    }
}

#[test]
fn deduped_wide_wave_settles_each_instance_correctly() {
    let _serial = scheduler_test_serial();
    for iter in 0..ITERS_B {
        let base = iter * 1_000;
        let mut sources = Vec::with_capacity(N_B);
        let mut derived = Vec::with_capacity(N_B);
        for _ in 0..N_B {
            let s = Cell::new(0i64);
            let d = s.clone().deduped().materialize();
            sources.push(s);
            derived.push(d);
        }
        batch(|| {
            for (k, s) in sources.iter().enumerate() {
                s.set(base + k as i64 + 1);
            }
        });
        for k in 0..N_B {
            let v = base + k as i64 + 1;
            assert_eq!(
                derived[k].get(),
                v,
                "deduped instance {k} settled wrong at iter {iter}"
            );
        }
    }
}

#[test]
fn state_transition_wide_wave_settles_each_instance_correctly() {
    let _serial = scheduler_test_serial();
    // 16 independent state machines, each with one defined transition
    // 0 -> 1 emitting an instance-distinct value. Driving all 16 sources to 1
    // in one wave, every machine must emit exactly its own value.
    for iter in 0..ITERS_B {
        let mut sources = Vec::with_capacity(N_B);
        let mut machines = Vec::with_capacity(N_B);
        for k in 0..N_B {
            let kk = k as i64;
            let s = Cell::new(0i64);
            let sm: Cell<i64, CellImmutable> = s.state_transition(move |sm| {
                sm.on(0i64, 1i64, move |_, _| (kk + 1) * 10);
            });
            sources.push(s);
            machines.push(sm);
        }
        batch(|| {
            for s in &sources {
                s.set(1);
            }
        });
        for k in 0..N_B {
            assert_eq!(
                machines[k].get(),
                (k as i64 + 1) * 10,
                "state_transition instance {k} emitted wrong at iter {iter}"
            );
        }
    }
}

#[test]
fn map_ok_wide_wave_settles_each_instance_correctly() {
    let _serial = scheduler_test_serial();
    // Even k: source set to Ok(v) -> map_ok multiplies. Odd k: source set to
    // Err -> map_ok forwards the Err untouched. Both variants per wave.
    for iter in 0..ITERS_B {
        let base = iter * 1_000;
        let mut sources = Vec::with_capacity(N_B);
        let mut derived = Vec::with_capacity(N_B);
        for k in 0..N_B {
            let kk = k as i64;
            let s = Cell::new(Ok::<i64, String>(0));
            let d = s.clone().map_ok(move |v| v * (kk + 1)).materialize();
            sources.push(s);
            derived.push(d);
        }
        batch(|| {
            for (k, s) in sources.iter().enumerate() {
                if k % 2 == 0 {
                    s.set(Ok(base + k as i64 + 1));
                } else {
                    s.set(Err(format!("e{k}")));
                }
            }
        });
        for k in 0..N_B {
            let expected: Result<i64, String> = if k % 2 == 0 {
                Ok((base + k as i64 + 1) * (k as i64 + 1))
            } else {
                Err(format!("e{k}"))
            };
            assert_eq!(
                derived[k].get(),
                expected,
                "map_ok instance {k} settled wrong at iter {iter}"
            );
        }
    }
}

#[test]
fn map_err_wide_wave_settles_each_instance_correctly() {
    let _serial = scheduler_test_serial();
    // Odd k: source set to Err -> map_err rewrites the error. Even k: source
    // set to Ok -> map_err forwards the Ok untouched. Both variants per wave.
    for iter in 0..ITERS_B {
        let base = iter * 1_000;
        let mut sources = Vec::with_capacity(N_B);
        let mut derived = Vec::with_capacity(N_B);
        for k in 0..N_B {
            let kk = k as i64;
            let s = Cell::new(Err::<i64, String>("init".into()));
            let d = s
                .clone()
                .map_err(move |e| format!("{e}!{kk}"))
                .materialize();
            sources.push(s);
            derived.push(d);
        }
        batch(|| {
            for (k, s) in sources.iter().enumerate() {
                if k % 2 == 0 {
                    s.set(Ok(base + k as i64 + 1));
                } else {
                    s.set(Err(format!("e{k}")));
                }
            }
        });
        for k in 0..N_B {
            let expected: Result<i64, String> = if k % 2 == 0 {
                Ok(base + k as i64 + 1)
            } else {
                Err(format!("e{k}!{k}"))
            };
            assert_eq!(
                derived[k].get(),
                expected,
                "map_err instance {k} settled wrong at iter {iter}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Template C — event order under no_coalesce parallel wave (expect FAIL)
// ---------------------------------------------------------------------------

/// Drive `src` with the in-order burst 1,2,...,N_C in one batch, `ITERS_C`
/// times, and assert `derived` delivered those events one-at-a-time and in
/// arrival order. `src` and `derived` must both have been built inside a
/// `no_coalesce` scope so neither the source events nor the operator's
/// per-event emissions are last-write-wins-coalesced away.
///
/// This is the confirmed-bug detector: once the burst crosses the parallel
/// wave threshold, the wave dispatches the events across rayon, so both the
/// order assert and the "never concurrent with itself" assert are expected to
/// fail until the scheduler makes `no_coalesce` waves order-preserving.
/// (in_flight / order detector copied from `scheduler_no_coalesce_wave_order.rs`.)
fn assert_ordered_event_delivery(
    label: &str,
    src: Cell<i64, CellMutable>,
    derived: Cell<i64, CellImmutable>,
) {
    let received: Arc<Mutex<Vec<i64>>> = Arc::new(Mutex::new(Vec::new()));
    let in_flight = Arc::new(AtomicUsize::new(0));
    let max_in_flight = Arc::new(AtomicUsize::new(0));

    let r = received.clone();
    let inf = in_flight.clone();
    let maxf = max_in_flight.clone();
    let guard = derived.subscribe(move |sig| {
        if let Signal::Value(v) = sig {
            // If the same subscriber runs concurrently with itself, in_flight
            // climbs above 1.
            let cur = inf.fetch_add(1, Ordering::SeqCst) + 1;
            maxf.fetch_max(cur, Ordering::SeqCst);
            // Widen the window so a genuine overlap is actually observed.
            for _ in 0..64 {
                std::hint::spin_loop();
            }
            r.lock().unwrap().push(**v);
            inf.fetch_sub(1, Ordering::SeqCst);
        }
    });

    let expected: Vec<i64> = (1..=N_C).collect();
    for it in 0..ITERS_C {
        received.lock().unwrap().clear();
        batch(|| {
            for i in 1..=N_C {
                src.set(i);
            }
        });
        let got = received.lock().unwrap().clone();
        assert_eq!(
            got.len(),
            N_C as usize,
            "{label} iter {it}: a no_coalesce event was dropped/coalesced"
        );
        assert_eq!(
            got, expected,
            "{label} iter {it}: no_coalesce events delivered OUT OF ORDER under a parallel wave"
        );
    }

    std::mem::forget(guard);
    let peak = max_in_flight.load(Ordering::SeqCst);
    assert_eq!(
        peak, 1,
        "{label}: derived event subscriber ran concurrently with itself (peak in-flight = {peak})"
    );
}

#[test]
fn scan_event_order_under_parallel_wave_regression() {
    let _serial = scheduler_test_serial();
    let (src, derived) = no_coalesce(|| {
        let src = Cell::<i64, CellMutable>::new(0);
        // Pass-through fold: emits each arriving input, so the delivered
        // sequence *is* the arrival order the scan closure processed events in.
        let derived = src.clone().scan(0i64, |_acc, x| *x).materialize();
        (src, derived)
    });
    assert_ordered_event_delivery("scan", src, derived);
}

#[test]
fn distinct_until_changed_by_event_order_under_parallel_wave_regression() {
    let _serial = scheduler_test_serial();
    let (src, derived) = no_coalesce(|| {
        let src = Cell::<i64, CellMutable>::new(0);
        // Inputs 1..=N_C are all distinct, so every event passes the (==)
        // comparator; the delivered sequence is exactly the arrival order.
        let derived = src
            .clone()
            .distinct_until_changed_by(|a, b| a == b)
            .materialize();
        (src, derived)
    });
    assert_ordered_event_delivery("distinct_until_changed_by", src, derived);
}

#[test]
fn deduped_event_order_under_parallel_wave_regression() {
    let _serial = scheduler_test_serial();
    let (src, derived) = no_coalesce(|| {
        let src = Cell::<i64, CellMutable>::new(0);
        // Distinct inputs -> every event passes deduped; delivered sequence is
        // the arrival order.
        let derived = src.clone().deduped().materialize();
        (src, derived)
    });
    assert_ordered_event_delivery("deduped", src, derived);
}

#[test]
fn state_transition_event_order_under_parallel_wave_regression() {
    let _serial = scheduler_test_serial();
    let (src, derived) = no_coalesce(|| {
        let src = Cell::<i64, CellMutable>::new(0);
        // A cyclic state machine: (0->1->2->...->N_C->1) with every step
        // defined and emitting its target value. Driven in order 1,2,...,N_C
        // each batch, it emits exactly 1,2,...,N_C; the cycle edge (N_C->1)
        // keeps that true across batches even though the state persists.
        let derived: Cell<i64, CellImmutable> = src.state_transition(|sm| {
            for k in 0..N_C {
                sm.on(k, k + 1, move |_, to| *to);
            }
            sm.on(N_C, 1i64, move |_, to| *to);
        });
        (src, derived)
    });
    assert_ordered_event_delivery("state_transition", src, derived);
}
