//! Wide-parallel-wave stress tests for hyphae's multi-input / flattening
//! combining operators under the scheduler's `run_wave` drain.
//!
//! Shared shape (Template A, copied from `scheduler_zip_mispair.rs` /
//! `scheduler_map_join_torn.rs`): drive `WIDTH` (>= `PARALLEL_WAVE_THRESHOLD`)
//! independent operator instances inside ONE `batch()`, so a single tick
//! produces `2 * WIDTH` height-0 ops — comfortably over the threshold (8) that
//! makes `run_wave` dispatch the wave across rayon. Each operator's own two
//! driving cells (distinct ids, same height) then run genuinely concurrently on
//! different threads, which is the precondition every hazard below needs.
//!
//! Per-operator hazard targeted:
//!
//! * `merge` (`concurrent_merges_never_drop_or_duplicate_and_complete_once`):
//!   two input sinks forward into ONE output cell. Under a wide wave both sinks
//!   `notify` that shared output at the literal same instant, and both source
//!   completes race the `AtomicU8` two-must-complete handshake. Asserts every
//!   emission survives exactly once (event semantics via `no_coalesce`) and
//!   `Complete` fires exactly once. Expected SAFE — merge only forwards, it
//!   never reads sibling state to build a value, and the completion handshake
//!   is a single atomic `fetch_or`.
//!
//! * `merge_map` (`concurrent_merge_maps_complete_exactly_once`): completion
//!   fires when the outer completes AND all inners complete, decided by a
//!   non-atomic check-then-act across an `AtomicBool` (outer_complete) and an
//!   `AtomicUsize` (active_inners). When the final inner's complete and the
//!   outer's complete land in the same parallel wave, both sides can observe
//!   the other's flag already set and BOTH call `notify(Complete)` — a
//!   duplicate terminal. Observed via a `no_coalesce` output (coalescing would
//!   mask the second `Complete`). Expected to FAIL (duplicate Complete).
//!
//! * `switch_map` (`concurrent_switch_map_latest_inner_wins_same_height`): the
//!   SAME-height parallel case `scheduler_phase0`'s
//!   `switch_map_old_inner_firing_during_switch_is_glitch_free` does not cover
//!   (that one makes the new inner *taller*). Here the selector and the old
//!   inner are both height-0 sources, so they collide in one wide wave. If the
//!   old inner reads the generation counter *before* the selector's CAS bumps
//!   it, it slips past the staleness gate and notifies the old value; if that
//!   notify then wins the output's last-write-wins coalescing slot over the
//!   switched-in inner's, the result settles STALE. Expected to FAIL
//!   (stale-inner survivor), though the window is narrow — the generation gate
//!   plus seq-ordering catches most interleavings.
#![cfg(feature = "scheduler")]

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use hyphae::{
    Cell, Gettable, MergeExt, MergeMapExt, Mutable, Signal, SwitchMapExt, Watchable, batch,
    scheduler::no_coalesce,
};

/// Width of every wave. Must be >= `PARALLEL_WAVE_THRESHOLD` (8) so the drain
/// dispatches across rayon; `2 * WIDTH` height-0 ops per batch clears it twice
/// over.
const WIDTH: usize = 8;
const ITERATIONS: i64 = 3_000;

/// The scheduler's tick queue is process-wide, so `cargo test`'s default of
/// running every `#[test]` in a binary concurrently would let these batches
/// interfere. Serialize them (same rationale as `scheduler_phase0.rs`).
fn scheduler_test_serial() -> std::sync::MutexGuard<'static, ()> {
    // Force the wave-parallel drain path at test width: production defaults
    // the group threshold high (waves stay sequential at rest), so parallelism
    // tests must lower it to actually exercise concurrent same-height groups.
    hyphae::scheduler::set_wave_threshold_for_test(4);
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

// ============================================================================
// merge
// ============================================================================

#[test]
fn concurrent_merges_never_drop_or_duplicate_and_complete_once() {
    let _serial = scheduler_test_serial();

    // Event semantics: `no_coalesce` the sources AND the merge output so a
    // wide wave's two concurrent forwards into one output both survive (a
    // coalescing output would last-write-wins one away and hide a real drop).
    let mut lefts = Vec::new();
    let mut rights = Vec::new();
    let mut received = Vec::new();
    let mut completes = Vec::new();
    let mut guards = Vec::new();

    for _ in 0..WIDTH {
        let (l, r, m) = no_coalesce(|| {
            let l = Cell::new(0i64);
            let r = Cell::new(0i64);
            let m = l.merge(&r);
            (l, r, m)
        });

        let got = Arc::new(Mutex::new(Vec::<i64>::new()));
        let done = Arc::new(AtomicUsize::new(0));
        let (g, d) = (got.clone(), done.clone());
        let guard = m.subscribe(move |sig| match sig {
            Signal::Value(v) => g.lock().unwrap().push(**v),
            Signal::Complete => {
                d.fetch_add(1, Ordering::SeqCst);
            }
            Signal::Error(_) => {}
        });

        lefts.push(l);
        rights.push(r);
        received.push(got);
        completes.push(done);
        guards.push(guard);
    }

    for it in 1..=ITERATIONS {
        // Drop any construction-time / prior-iteration emissions.
        for buf in &received {
            buf.lock().unwrap().clear();
        }

        // 2 * WIDTH height-0 sets in one batch → a forced parallel wave; each
        // merge's left and right forward into its single output concurrently.
        batch(|| {
            #[allow(clippy::needless_range_loop)]
            for i in 0..WIDTH {
                let base = it * 1000 + (i as i64) * 2;
                lefts[i].set(base);
                rights[i].set(base + 1);
            }
        });

        #[allow(clippy::needless_range_loop)]
        for i in 0..WIDTH {
            let base = it * 1000 + (i as i64) * 2;
            let mut got = received[i].lock().unwrap().clone();
            got.sort_unstable();
            assert_eq!(
                got,
                vec![base, base + 1],
                "merge dropped/duplicated/mis-forwarded an emission at iteration {it}, merge {i}"
            );
        }
    }

    // Completion handshake under a wide wave: complete both sides of all merges
    // in one batch (2 * WIDTH height-0 completes). Each merge's two source
    // completes race the AtomicU8 two-must-complete flag concurrently.
    batch(|| {
        #[allow(clippy::needless_range_loop)]
        for i in 0..WIDTH {
            lefts[i].complete();
            rights[i].complete();
        }
    });
    #[allow(clippy::needless_range_loop)]
    for i in 0..WIDTH {
        assert_eq!(
            completes[i].load(Ordering::SeqCst),
            1,
            "merge {i} fired Complete {} times (expected exactly 1)",
            completes[i].load(Ordering::SeqCst)
        );
    }

    drop(guards);
}

// ============================================================================
// merge_map
// ============================================================================

#[test]
fn concurrent_merge_maps_complete_exactly_once() {
    let _serial = scheduler_test_serial();

    // Completion is terminal, so each iteration builds fresh operators. Only
    // completes are driven (no values), so `active_inners` stays at 1 (the
    // first inner) and the outer-complete vs final-inner-complete race is the
    // sole path to the terminal.
    for it in 0..ITERATIONS {
        let mut outers = Vec::new();
        let mut inner_srcs = Vec::new();
        let mut completes = Vec::new();
        let mut guards = Vec::new();

        no_coalesce(|| {
            for _ in 0..WIDTH {
                let outer = Cell::new(0i64);
                let inner_src = Cell::new(0i64);
                let ic = inner_src.clone();
                // First (and only) inner is the `inner_src` cell itself, locked
                // immutable — height 0, same as the outer, so completing both in
                // one batch puts them in the same wide wave.
                let mm = outer.merge_map(move |_: &i64| ic.clone().lock());

                let done = Arc::new(AtomicUsize::new(0));
                let d = done.clone();
                let guard = mm.subscribe(move |sig| {
                    if matches!(sig, Signal::Complete) {
                        d.fetch_add(1, Ordering::SeqCst);
                    }
                });

                outers.push(outer);
                inner_srcs.push(inner_src);
                completes.push(done);
                guards.push(guard);
            }
        });

        // 2 * WIDTH height-0 completes in one batch → parallel wave. For each
        // merge_map, its outer-complete and inner-complete callbacks run
        // concurrently: the check-then-act across `outer_complete` and
        // `active_inners` can let both decide to fire.
        batch(|| {
            #[allow(clippy::needless_range_loop)]
            for i in 0..WIDTH {
                outers[i].complete();
                inner_srcs[i].complete();
            }
        });

        #[allow(clippy::needless_range_loop)]
        for i in 0..WIDTH {
            assert_eq!(
                completes[i].load(Ordering::SeqCst),
                1,
                "merge_map fired Complete {} times at iteration {it}, unit {i} (expected exactly 1)",
                completes[i].load(Ordering::SeqCst)
            );
        }

        drop(guards);
    }
}

// ============================================================================
// switch_map
// ============================================================================

#[test]
fn concurrent_switch_map_latest_inner_wins_same_height() {
    let _serial = scheduler_test_serial();

    // Behavior (latest-value) semantics — leave coalescing ON, since the hazard
    // IS a stale value winning the last-write-wins slot. Each switch from
    // inner 0 -> inner 1 is a one-shot event (once generation bumps, the old
    // inner is gated out forever), so fresh operators every iteration.
    for it in 1..=ITERATIONS {
        let mut sels = Vec::new();
        let mut old_srcs = Vec::new();
        let mut results = Vec::new();

        for i in 0..WIDTH {
            let sel = Cell::new(0i64); // starts on inner 0 (the old inner)
            let old_src = Cell::new(0i64);
            let bval = it * 1000 + i as i64 + 500;
            let new_src = Cell::new(bval);

            // Both inners are bare locked sources at height 0 — the same height
            // as the selector, so selector-set and old-inner-set collide in one
            // wave. The old inner is the current inner until `sel` flips to 1.
            let old = old_src.clone().lock();
            let new = new_src.clone().lock();
            let result = sel.switch_map(move |&k| if k == 0 { old.clone() } else { new.clone() });

            sels.push(sel);
            old_srcs.push(old_src);
            results.push(result);
        }

        // In one batch, for every unit: fire the OLD inner AND switch to the
        // new inner. 2 * WIDTH height-0 ops → parallel wave, so each unit's
        // old-inner emission races its selector's generation bump.
        batch(|| {
            #[allow(clippy::needless_range_loop)]
            for i in 0..WIDTH {
                let aval = it * 1000 + i as i64; // old inner's stale value
                old_srcs[i].set(aval);
                sels[i].set(1); // switch to the new inner
            }
        });

        #[allow(clippy::needless_range_loop)]
        for i in 0..WIDTH {
            let bval = it * 1000 + i as i64 + 500;
            assert_eq!(
                results[i].get(),
                bval,
                "switch_map settled on a STALE old-inner value at iteration {it}, unit {i} \
                 (expected the switched-in inner's {bval})"
            );
        }
    }
}
