//! Regression harness: single-input slicing/stateful operators under the
//! scheduler's wave-parallel drain.
//!
//! The `scheduler` feature's `batch(|| ...)` defers notifies into a
//! height-ordered queue. A wave of `>= PARALLEL_WAVE_THRESHOLD` (8) same-height
//! ops dispatches across rayon IN PARALLEL. This file pins down two distinct
//! properties for the single-input operators in
//! `src/traits/operators/{take,take_while,take_until,skip,skip_while,first,
//! last,pairwise,buffer_count,window}.rs`:
//!
//! Template B — WIDE INDEPENDENT WAVE (expected: PASS).
//!   Each operator instance owns its OWN source. Driving all `WIDE` sources in
//!   one `batch()` makes them `WIDE` distinct-id ops at height 0 — a genuinely
//!   parallel wave that runs each instance's operator fanout concurrently on a
//!   different worker. A single-input operator sees exactly one value per wave
//!   (its source is a coalescing cell set once per batch), so there is no
//!   two-sibling torn race; the only thing a wide wave can break is
//!   CROSS-INSTANCE corruption. Each instance's output is a pure function of its
//!   own source, so the correct settled sequence is exactly computable and
//!   asserted per instance.
//!
//! Template C — EVENT ORDER on a `no_coalesce` burst (expected: FAIL until the
//!   scheduler's same-cell wave ordering is fixed — this is the confirmed bug).
//!   A `no_coalesce` (event) source `set` N (>= 8) times inside ONE batch
//!   becomes N ops sharing `(height=0, id)` that all land in the SAME wave and
//!   dispatch in parallel — reordering the events. Order-sensitive operators
//!   built on such a source (`pairwise`, `buffer_count`, `window`) are exposed:
//!   their stateful fold runs against a scrambled event order and produces
//!   wrong pairs / chunks / windows. Building the source AND the operator's
//!   materialized cell together inside `no_coalesce(|| ...)` stamps BOTH as
//!   event-semantic, so every downstream emission is delivered uncoalesced and
//!   the full emitted sequence is observable and asserted exactly.
//!
//! NOTE: `take_until` is two-input (source + notifier). Its Template-B wide wave
//! is the SOURCE fanout across `WIDE` independent sources; the stop is driven by
//! a separate (also wide) notifier wave. There is no way to fold the notifier
//! arm into the same same-height source wave via the public API, so the source
//! wave is what this harness stresses.
#![cfg(feature = "scheduler")]
#![allow(clippy::needless_range_loop)] // deliberate parallel-vec indexing

use std::sync::{Arc, Mutex};

use hyphae::{
    BufferCountExt, Cell, CellMutable, FirstExt, Gettable, LastExt, MaterializeDefinite,
    MaterializeEmpty, Mutable, PairwiseExt, Signal, SkipExt, SkipWhileExt, TakeExt, TakeUntilExt,
    TakeWhileExt, Watchable, WindowExt, batch, scheduler::no_coalesce,
};

/// The scheduler's tick queue is a single process-wide structure, so `#[test]`
/// fns (which `cargo test` runs as concurrent threads) would otherwise smear
/// each other's batches together. Serialize every test in this file.
fn scheduler_test_serial() -> std::sync::MutexGuard<'static, ()> {
    // Force the wave-parallel drain path at test width: production defaults
    // the group threshold high (waves stay sequential at rest), so parallelism
    // tests must lower it to actually exercise concurrent same-height groups.
    hyphae::scheduler::set_wave_threshold_for_test(4);
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Number of independent operator instances driven per wide wave. Strictly
/// greater than the scheduler's PARALLEL_WAVE_THRESHOLD (8), so every batch
/// below forces a genuinely parallel wave, never the small-wave sequential
/// fallback.
const WIDE: usize = 16;
/// Sets driven per Template-B instance (well past every operator's boundary).
const ITERS: i64 = 2_000;

/// Events fired at one `no_coalesce` source inside a SINGLE batch for the
/// Template-C order tests — comfortably over the parallel-wave threshold.
const BURST: i64 = 32;
/// Batches of `BURST` events for the Template-C order tests.
const ORDER_ITERS: usize = 2_000;

// ---------------------------------------------------------------------------
// Template B — wide independent same-height waves. Expected: PASS.
// ---------------------------------------------------------------------------

#[test]
fn take_wide_independent_wave_each_instance_exact() {
    let _serial = scheduler_test_serial();

    let sources: Vec<Cell<i64, CellMutable>> = (0..WIDE).map(|_| Cell::new(0i64)).collect();
    // Distinct caps so the boundary lands at a different point per instance.
    let counts: Vec<usize> = (0..WIDE).map(|k| k + 2).collect();
    let cells: Vec<_> = sources
        .iter()
        .zip(&counts)
        .map(|(s, &c)| s.clone().take(c).materialize())
        .collect();

    let recv: Vec<Arc<Mutex<Vec<i64>>>> = (0..WIDE)
        .map(|_| Arc::new(Mutex::new(Vec::new())))
        .collect();
    let mut guards = Vec::new();
    for (cell, r) in cells.iter().zip(&recv) {
        let rr = r.clone();
        guards.push(cell.subscribe(move |sig| {
            if let Signal::Value(v) = sig {
                rr.lock().unwrap().push(**v);
            }
        }));
    }

    // One value delivered to every instance per wide parallel wave.
    for i in 1..=ITERS {
        batch(|| {
            for s in &sources {
                s.set(i);
            }
        });
    }

    for k in 0..WIDE {
        // take(c): the synchronous subscribe-time emit (value 0) is the 1st of
        // c, then values 1..=c-1 land — exactly [0, 1, ..., c-1], then complete.
        let expected: Vec<i64> = (0..counts[k] as i64).collect();
        assert_eq!(
            *recv[k].lock().unwrap(),
            expected,
            "take instance {k} (count {}) settled wrong under the wide parallel wave",
            counts[k]
        );
        assert!(cells[k].is_complete(), "take instance {k} never completed");
    }
    drop(guards);
}

#[test]
fn take_while_wide_independent_wave_each_instance_exact() {
    let _serial = scheduler_test_serial();

    let sources: Vec<Cell<i64, CellMutable>> = (0..WIDE).map(|_| Cell::new(0i64)).collect();
    let thresholds: Vec<i64> = (0..WIDE).map(|k| k as i64 + 3).collect();
    let cells: Vec<_> = sources
        .iter()
        .zip(&thresholds)
        .map(|(s, &th)| s.clone().take_while(move |x| *x < th).materialize())
        .collect();

    let recv: Vec<Arc<Mutex<Vec<i64>>>> = (0..WIDE)
        .map(|_| Arc::new(Mutex::new(Vec::new())))
        .collect();
    let mut guards = Vec::new();
    for (cell, r) in cells.iter().zip(&recv) {
        let rr = r.clone();
        guards.push(cell.subscribe(move |sig| {
            if let Signal::Value(v) = sig {
                // Empty-materialized: payload is Option<i64>; the initial
                // subscribe replay is None. Only collect real values.
                if let Some(x) = **v {
                    rr.lock().unwrap().push(x);
                }
            }
        }));
    }

    for i in 1..=ITERS {
        batch(|| {
            for s in &sources {
                s.set(i);
            }
        });
    }

    for k in 0..WIDE {
        // take_while(x < th): passes 0..th-1, then completes at th and freezes.
        let expected: Vec<i64> = (0..thresholds[k]).collect();
        assert_eq!(
            *recv[k].lock().unwrap(),
            expected,
            "take_while instance {k} (th {}) settled wrong under the wide parallel wave",
            thresholds[k]
        );
        assert_eq!(
            cells[k].get(),
            Some(thresholds[k] - 1),
            "take_while instance {k} froze on the wrong last-passing value"
        );
        assert!(
            cells[k].is_complete(),
            "take_while instance {k} never completed"
        );
    }
    drop(guards);
}

#[test]
fn take_until_wide_independent_wave_each_instance_exact() {
    let _serial = scheduler_test_serial();

    let sources: Vec<Cell<i64, CellMutable>> = (0..WIDE).map(|_| Cell::new(0i64)).collect();
    let stoppers: Vec<Cell<bool, CellMutable>> = (0..WIDE).map(|_| Cell::new(false)).collect();
    let cells: Vec<_> = sources
        .iter()
        .zip(&stoppers)
        .map(|(s, st)| s.take_until(st))
        .collect();

    // Wide source wave: each instance tracks the latest value of its own source.
    for i in 1..=ITERS {
        batch(|| {
            for s in &sources {
                s.set(i);
            }
        });
    }
    // Wide notifier wave: stop every instance at once.
    batch(|| {
        for st in &stoppers {
            st.set(true);
        }
    });
    // Post-stop source waves must be ignored by every instance.
    for i in ITERS + 1..=ITERS + 50 {
        batch(|| {
            for s in &sources {
                s.set(i);
            }
        });
    }

    for k in 0..WIDE {
        assert_eq!(
            cells[k].get(),
            ITERS,
            "take_until instance {k} did not freeze at the pre-stop value under the wide wave"
        );
        assert!(
            cells[k].is_complete(),
            "take_until instance {k} never completed"
        );
    }
}

#[test]
fn skip_wide_independent_wave_each_instance_exact() {
    let _serial = scheduler_test_serial();

    let sources: Vec<Cell<i64, CellMutable>> = (0..WIDE).map(|_| Cell::new(0i64)).collect();
    let skips: Vec<usize> = (0..WIDE).map(|k| k + 1).collect();
    let cells: Vec<_> = sources
        .iter()
        .zip(&skips)
        .map(|(s, &n)| s.clone().skip(n).materialize())
        .collect();

    let recv: Vec<Arc<Mutex<Vec<i64>>>> = (0..WIDE)
        .map(|_| Arc::new(Mutex::new(Vec::new())))
        .collect();
    let mut guards = Vec::new();
    for (cell, r) in cells.iter().zip(&recv) {
        let rr = r.clone();
        guards.push(cell.subscribe(move |sig| {
            if let Signal::Value(v) = sig
                && let Some(x) = (**v)
            {
                rr.lock().unwrap().push(x);
            }
        }));
    }

    for i in 1..=ITERS {
        batch(|| {
            for s in &sources {
                s.set(i);
            }
        });
    }

    for k in 0..WIDE {
        // skip(n): the synchronous subscribe-time emit (value 0) is the 1st of
        // n skips, so the first passed value is n; passes [n, n+1, ..., ITERS].
        let n = skips[k] as i64;
        let expected: Vec<i64> = (n..=ITERS).collect();
        assert_eq!(
            *recv[k].lock().unwrap(),
            expected,
            "skip instance {k} (n {}) settled wrong under the wide parallel wave",
            skips[k]
        );
    }
    drop(guards);
}

#[test]
fn skip_while_wide_independent_wave_each_instance_exact() {
    let _serial = scheduler_test_serial();

    let sources: Vec<Cell<i64, CellMutable>> = (0..WIDE).map(|_| Cell::new(0i64)).collect();
    let thresholds: Vec<i64> = (0..WIDE).map(|k| k as i64 + 1).collect();
    let cells: Vec<_> = sources
        .iter()
        .zip(&thresholds)
        .map(|(s, &th)| s.clone().skip_while(move |x| *x < th).materialize())
        .collect();

    let recv: Vec<Arc<Mutex<Vec<i64>>>> = (0..WIDE)
        .map(|_| Arc::new(Mutex::new(Vec::new())))
        .collect();
    let mut guards = Vec::new();
    for (cell, r) in cells.iter().zip(&recv) {
        let rr = r.clone();
        guards.push(cell.subscribe(move |sig| {
            if let Signal::Value(v) = sig
                && let Some(x) = (**v)
            {
                rr.lock().unwrap().push(x);
            }
        }));
    }

    for i in 1..=ITERS {
        batch(|| {
            for s in &sources {
                s.set(i);
            }
        });
    }

    for k in 0..WIDE {
        // skip_while(x < th): skips 0..th-1 (gate closed), then value th opens
        // the gate permanently — passes [th, th+1, ..., ITERS].
        let th = thresholds[k];
        let expected: Vec<i64> = (th..=ITERS).collect();
        assert_eq!(
            *recv[k].lock().unwrap(),
            expected,
            "skip_while instance {k} (th {th}) settled wrong under the wide parallel wave"
        );
    }
    drop(guards);
}

#[test]
fn first_wide_independent_wave_each_instance_exact() {
    let _serial = scheduler_test_serial();

    let sources: Vec<Cell<i64, CellMutable>> = (0..WIDE).map(|_| Cell::new(0i64)).collect();
    let cells: Vec<_> = sources
        .iter()
        .map(|s| s.clone().first().materialize())
        .collect();

    let recv: Vec<Arc<Mutex<Vec<i64>>>> = (0..WIDE)
        .map(|_| Arc::new(Mutex::new(Vec::new())))
        .collect();
    let mut guards = Vec::new();
    for (cell, r) in cells.iter().zip(&recv) {
        let rr = r.clone();
        guards.push(cell.subscribe(move |sig| {
            if let Signal::Value(v) = sig {
                rr.lock().unwrap().push(**v);
            }
        }));
    }

    // Every instance completed at subscribe (first == take(1)); the wide wave
    // then delivers to already-exhausted operators, which must stay untouched.
    for i in 1..=ITERS {
        batch(|| {
            for s in &sources {
                s.set(i);
            }
        });
    }

    for k in 0..WIDE {
        assert_eq!(
            *recv[k].lock().unwrap(),
            vec![0i64],
            "first instance {k} emitted more than the initial value under the wide wave"
        );
        assert!(cells[k].is_complete(), "first instance {k} never completed");
    }
    drop(guards);
}

#[test]
fn last_wide_independent_wave_each_instance_exact() {
    let _serial = scheduler_test_serial();

    let sources: Vec<Cell<i64, CellMutable>> = (0..WIDE).map(|_| Cell::new(0i64)).collect();
    let cells: Vec<_> = sources
        .iter()
        .map(|s| s.clone().last().materialize())
        .collect();

    let recv: Vec<Arc<Mutex<Vec<i64>>>> = (0..WIDE)
        .map(|_| Arc::new(Mutex::new(Vec::new())))
        .collect();
    let mut guards = Vec::new();
    for (cell, r) in cells.iter().zip(&recv) {
        let rr = r.clone();
        guards.push(cell.subscribe(move |sig| {
            if let Signal::Value(v) = sig
                && let Some(x) = (**v)
            {
                rr.lock().unwrap().push(x);
            }
        }));
    }

    for i in 1..=ITERS {
        batch(|| {
            for s in &sources {
                s.set(i);
            }
        });
    }
    // Wide completion wave: last() emits only its most recent value on Complete.
    batch(|| {
        for s in &sources {
            s.complete();
        }
    });

    for k in 0..WIDE {
        assert_eq!(
            *recv[k].lock().unwrap(),
            vec![ITERS],
            "last instance {k} emitted the wrong most-recent value under the wide wave"
        );
        assert_eq!(
            cells[k].get(),
            Some(ITERS),
            "last instance {k} settled wrong"
        );
        assert!(cells[k].is_complete(), "last instance {k} never completed");
    }
    drop(guards);
}

// ---------------------------------------------------------------------------
// Template C — event order on a `no_coalesce` wide burst. Expected: FAIL until
// the scheduler stops reordering same-cell event ops within a parallel wave.
// These assert the CORRECT (in-order) result, so they pass once fixed.
// ---------------------------------------------------------------------------

#[test]
fn pairwise_no_coalesce_burst_keeps_pairs_in_order() {
    let _serial = scheduler_test_serial();

    // Source AND the pairwise output cell born inside `no_coalesce`, so every
    // (prev, cur) pair is delivered uncoalesced — the full emitted sequence is
    // observable.
    let (src, pairs) = no_coalesce(|| {
        let src = Cell::<i64, CellMutable>::new(0);
        let pairs = src.clone().pairwise().materialize();
        (src, pairs)
    });

    let received: Arc<Mutex<Vec<(i64, i64)>>> = Arc::new(Mutex::new(Vec::new()));
    let r = received.clone();
    let guard = pairs.subscribe(move |sig| {
        if let Signal::Value(v) = sig
            && let Some(pair) = (**v)
        {
            r.lock().unwrap().push(pair);
        }
    });

    // Monotonic values across the whole run: pairwise's `last` carries across
    // batches, so a contiguous value stream keeps the expected pair exactly
    // (v-1, v) for every emitted v — no per-batch wraparound to special-case.
    let mut next = 1i64;
    for it in 0..ORDER_ITERS {
        received.lock().unwrap().clear();
        let lo = next;
        batch(|| {
            for _ in 0..BURST {
                src.set(next);
                next += 1;
            }
        });
        let hi = next - 1;
        let expected: Vec<(i64, i64)> = (lo..=hi).map(|x| (x - 1, x)).collect();
        let got = received.lock().unwrap().clone();
        assert_eq!(
            got, expected,
            "iter {it}: pairwise produced REORDERED pairs under the parallel no_coalesce wave"
        );
    }
    std::mem::forget(guard);
}

#[test]
fn buffer_count_no_coalesce_burst_keeps_chunks_in_order() {
    let _serial = scheduler_test_serial();

    const COUNT: usize = 4; // BURST is a multiple of COUNT -> no cross-batch remainder.

    let (src, buffered) = no_coalesce(|| {
        let src = Cell::<i64, CellMutable>::new(0);
        let buffered = src.buffer_count(COUNT);
        (src, buffered)
    });

    let received: Arc<Mutex<Vec<Vec<i64>>>> = Arc::new(Mutex::new(Vec::new()));
    let r = received.clone();
    let guard = buffered.subscribe(move |sig| {
        if let Signal::Value(v) = sig {
            r.lock().unwrap().push((**v).clone());
        }
    });

    let mut next = 1i64;
    for it in 0..ORDER_ITERS {
        received.lock().unwrap().clear();
        let lo = next;
        batch(|| {
            for _ in 0..BURST {
                src.set(next);
                next += 1;
            }
        });
        let hi = next - 1;
        // Contiguous values chunked into non-overlapping groups of COUNT.
        let vals: Vec<i64> = (lo..=hi).collect();
        let expected: Vec<Vec<i64>> = vals.chunks(COUNT).map(|c| c.to_vec()).collect();
        let got = received.lock().unwrap().clone();
        assert_eq!(
            got, expected,
            "iter {it}: buffer_count produced REORDERED/torn chunks under the parallel no_coalesce wave"
        );
    }
    std::mem::forget(guard);
}

#[test]
fn window_no_coalesce_burst_keeps_windows_in_order() {
    let _serial = scheduler_test_serial();

    const COUNT: i64 = 4;

    let (src, windowed) = no_coalesce(|| {
        let src = Cell::<i64, CellMutable>::new(0);
        let windowed = src.window(COUNT as usize);
        (src, windowed)
    });

    let received: Arc<Mutex<Vec<Vec<i64>>>> = Arc::new(Mutex::new(Vec::new()));
    let r = received.clone();
    let guard = windowed.subscribe(move |sig| {
        if let Signal::Value(v) = sig {
            r.lock().unwrap().push((**v).clone());
        }
    });

    // History fed to the sliding window is [0 (seed), 1, 2, 3, ...]; after value
    // x the window is the last COUNT of that history.
    let mut next = 1i64;
    for it in 0..ORDER_ITERS {
        received.lock().unwrap().clear();
        let lo = next;
        batch(|| {
            for _ in 0..BURST {
                src.set(next);
                next += 1;
            }
        });
        let hi = next - 1;
        let expected: Vec<Vec<i64>> = (lo..=hi)
            .map(|x| {
                let low = (x + 1 - COUNT).max(0);
                (low..=x).collect()
            })
            .collect();
        let got = received.lock().unwrap().clone();
        assert_eq!(
            got, expected,
            "iter {it}: window produced REORDERED/torn windows under the parallel no_coalesce wave"
        );
    }
    std::mem::forget(guard);
}
