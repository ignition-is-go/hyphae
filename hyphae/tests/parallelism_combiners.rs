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
//!   non-atomic check-then-act across an `AtomicBool` (`outer_complete`) and an
//!   `AtomicUsize` (`active_inners`). When the final inner's complete and the
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

use hyphae::{Cell, Gettable, Materialize, Mutable, SwitchMapExt, batch};

/// Width of every wave. Must be >= `PARALLEL_WAVE_THRESHOLD` (8) so the drain
/// dispatches across rayon; `2 * WIDTH` height-0 ops per batch clears it twice
/// over.
const WIDTH: usize = 8;
const ITERATIONS: i64 = 3_000;
static SCHEDULER_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn iteration_value(iteration: i64, index: usize, offset: i64) -> i64 {
    iteration
        .saturating_mul(1000)
        .saturating_add(i64::try_from(index).unwrap_or(i64::MAX).saturating_mul(2))
        .saturating_add(offset)
}

/// The scheduler's tick queue is process-wide, so `cargo test`'s default of
/// running every `#[test]` in a binary concurrently would let these batches
/// interfere. Serialize them (same rationale as `scheduler_phase0.rs`).
fn scheduler_test_serial() -> std::sync::MutexGuard<'static, ()> {
    // Force the wave-parallel drain path at test width: production defaults
    // the group threshold high (waves stay sequential at rest), so parallelism
    // tests must lower it to actually exercise concurrent same-height groups.
    SCHEDULER_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

// ============================================================================
// merge
// ============================================================================

// ============================================================================
// merge_map
// ============================================================================

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

        for index in 0..WIDTH {
            let sel = Cell::new(0i64); // starts on inner 0 (the old inner)
            let old_src = Cell::new(0i64);
            let new_value = iteration_value(it, index, 500);
            let new_src = Cell::new(new_value);

            // Both inners are bare locked sources at height 0 — the same height
            // as the selector, so selector-set and old-inner-set collide in one
            // wave. The old inner is the current inner until `sel` flips to 1.
            let old = old_src.clone().lock();
            let new = new_src.clone().lock();
            let result = sel
                .clone()
                .switch_map(move |&selector| {
                    if selector == 0 {
                        old.clone()
                    } else {
                        new.clone()
                    }
                })
                .materialize();

            sels.push(sel);
            old_srcs.push(old_src);
            results.push(result);
        }

        // In one batch, for every unit: fire the OLD inner AND switch to the
        // new inner. 2 * WIDTH height-0 ops → parallel wave, so each unit's
        // old-inner emission races its selector's generation bump.
        batch(|| {
            for (index, (old_source, selector)) in old_srcs.iter().zip(&sels).enumerate() {
                old_source.set(iteration_value(it, index, 0));
                selector.set(1); // switch to the new inner
            }
        });

        for (index, result) in results.iter().enumerate() {
            let new_value = iteration_value(it, index, 500);
            assert_eq!(
                result.get(),
                new_value,
                "switch_map settled on a STALE old-inner value at iteration {it}, unit {index} \
                 (expected the switched-in inner's {new_value})"
            );
        }
    }
}
