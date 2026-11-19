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
use std::sync::{
    Arc,
    atomic::{AtomicI64, Ordering},
};

use parking_lot::Mutex;

use hyphae::{
    Cell, CellImmutable, DedupedExt, DistinctExt, DistinctUntilChangedByExt, FilterExt, Gettable,
    MapErrExt, MapExt, MapOkExt, Materialize, Mutable, ScanExt, StateTransitionExt, TapExt,
    TryMapExt, batch,
};

/// The scheduler's tick queue is a single process-wide structure, so tests
/// that exercise `batch()` would interfere when `cargo test` runs this file's
/// `#[test]` fns as concurrent threads. Serialize them. (Copied from
/// `scheduler_completeness.rs`.)
static SCHEDULER_TEST_LOCK: Mutex<()> = Mutex::new(());

fn scheduler_test_serial() -> parking_lot::MutexGuard<'static, ()> {
    // Force the wave-parallel drain path at test width: production defaults
    // the group threshold high (waves stay sequential at rest), so parallelism
    // tests must lower it to actually exercise concurrent same-height groups.
    SCHEDULER_TEST_LOCK.lock()
}

fn index_value(index: usize) -> i64 {
    i64::try_from(index).unwrap_or(i64::MAX)
}

/// Template-B wave width: 16 > `PARALLEL_WAVE_THRESHOLD` (8), so every batch
/// forces a genuinely parallel wave rather than the small-wave sequential
/// fallback.
const N_B: usize = 16;
/// Template-B iteration count — enough to shake out wave nondeterminism.
const ITERS_B: i64 = 2_000;

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
            let kk = index_value(k);
            let s = Cell::new(0i64);
            let d = s.clone().map(move |x| x * (kk + 1) + 7).materialize();
            sources.push(s);
            derived.push(d);
        }
        batch(|| {
            for (k, s) in sources.iter().enumerate() {
                s.set(base + index_value(k) + 1);
            }
        });
        for (k, value) in derived.iter().enumerate() {
            let x = base + index_value(k) + 1;
            assert_eq!(
                value.get(),
                x * (index_value(k) + 1) + 7,
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
                s.set(base + index_value(k) + 1);
            }
        });
        for (k, value) in derived.iter().enumerate() {
            let v = base + index_value(k) + 1;
            let expected = if v % 2 == 0 { Some(v) } else { Some(0) };
            assert_eq!(
                value.get(),
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
                s.set(base + index_value(k) + 1);
            }
        });
        for (k, value) in derived.iter().enumerate() {
            let v = base + index_value(k) + 1;
            let expected: Result<i64, String> = if v % 2 == 0 {
                Ok(v * 2)
            } else {
                Err(format!("odd:{v}"))
            };
            assert_eq!(
                value.get(),
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
        for sc in &side {
            let s = Cell::new(0i64);
            let sc = sc.clone();
            let d = s
                .clone()
                .tap(move |v| sc.store(*v, Ordering::SeqCst))
                .materialize();
            sources.push(s);
            derived.push(d);
        }
        batch(|| {
            for (k, s) in sources.iter().enumerate() {
                s.set(base + index_value(k) + 1);
            }
        });
        for (k, (value, side_effect)) in derived.iter().zip(&side).enumerate() {
            let v = base + index_value(k) + 1;
            assert_eq!(value.get(), v, "tap value {k} wrong at iter {iter}");
            assert_eq!(
                side_effect.load(Ordering::SeqCst),
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
                s.set(base + index_value(k) + 1);
            }
        });
        for (k, value) in derived.iter().enumerate() {
            let v = base + index_value(k) + 1;
            assert_eq!(
                value.get(),
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
                s.set(base + index_value(k) + 1);
            }
        });
        for (k, value) in derived.iter().enumerate() {
            let v = base + index_value(k) + 1;
            assert_eq!(
                value.get(),
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
                s.set(base + index_value(k) + 1);
            }
        });
        for (k, value) in derived.iter().enumerate() {
            let v = base + index_value(k) + 1;
            assert_eq!(
                value.get(),
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
                s.set(base + index_value(k) + 1);
            }
        });
        for (k, value) in derived.iter().enumerate() {
            let v = base + index_value(k) + 1;
            assert_eq!(
                value.get(),
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
            let kk = index_value(k);
            let s = Cell::new(0i64);
            let sm: Cell<i64, CellImmutable> = s
                .clone()
                .state_transition(move |sm| {
                    sm.on(0i64, 1i64, move |_, _| (kk + 1) * 10);
                })
                .materialize();
            sources.push(s);
            machines.push(sm);
        }
        batch(|| {
            for s in &sources {
                s.set(1);
            }
        });
        for (k, machine) in machines.iter().enumerate() {
            assert_eq!(
                machine.get(),
                (index_value(k) + 1) * 10,
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
            let kk = index_value(k);
            let s = Cell::new(Ok::<i64, String>(0));
            let d = s.clone().map_ok(move |v| v * (kk + 1)).materialize();
            sources.push(s);
            derived.push(d);
        }
        batch(|| {
            for (k, s) in sources.iter().enumerate() {
                if k % 2 == 0 {
                    s.set(Ok(base + index_value(k) + 1));
                } else {
                    s.set(Err(format!("e{k}")));
                }
            }
        });
        for (k, value) in derived.iter().enumerate() {
            let expected: Result<i64, String> = if k % 2 == 0 {
                Ok((base + index_value(k) + 1) * (index_value(k) + 1))
            } else {
                Err(format!("e{k}"))
            };
            assert_eq!(
                value.get(),
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
            let kk = index_value(k);
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
                    s.set(Ok(base + index_value(k) + 1));
                } else {
                    s.set(Err(format!("e{k}")));
                }
            }
        });
        for (k, value) in derived.iter().enumerate() {
            let expected: Result<i64, String> = if k % 2 == 0 {
                Ok(base + index_value(k) + 1)
            } else {
                Err(format!("e{k}!{k}"))
            };
            assert_eq!(
                value.get(),
                expected,
                "map_err instance {k} settled wrong at iter {iter}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Template C — event order under no_coalesce parallel wave (expect FAIL)
// ---------------------------------------------------------------------------
