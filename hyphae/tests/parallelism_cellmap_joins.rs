//! Wave-parallel torn-value stress tests for the remaining CellMap join
//! operators: `left_join`, `left_semi_join`, and `multi_left_join_by`.
//! (`inner_join` is covered by `scheduler_map_join_torn.rs`.)
//!
//! Each of these is a two-input CellMap join: a left sink driven by
//! `left.diffs_cell` and a right sink driven by `right.diffs_cell`, both
//! height-0 roots. Each sink, when it recomputes an impacted left key, reads
//! the *sibling* side's rows (under the shared join-state `Mutex`) to build the
//! combined output row before pushing it into the single output cell.
//!
//! Under the scheduler's wave-parallel drain, a wave of >=
//! `PARALLEL_WAVE_THRESHOLD` (8) same-height ops dispatches on rayon, so a
//! join's own left op and right op can execute at the literal same instant on
//! two threads. If the combined-row emit is not ordered consistently with the
//! sibling read, two concurrent updates to the same output key can
//! last-write-wins a torn/stale row — a fresh value on one side paired with the
//! stale value from the other. The runtime fix (see
//! `internal/join_runtime.rs` and `internal/multi_join_runtime.rs`) holds the
//! state lock across the emit so emit order == lock order and whichever side
//! observed the freshest sibling emits last.
//!
//! To force a genuinely wide parallel wave, each test drives 8 independent join
//! pairs in ONE `batch()`: 8 pairs = 16 height-0 diff-cell ops, comfortably
//! over the threshold, so each join's own left+right ops run concurrently.
//! Every iteration sets a fresh, exactly-known value on both sides of every
//! pair, so the correct settled output is computable and checked strictly — a
//! torn survivor fails the assert.
#![cfg(feature = "scheduler")]

use hyphae::{
    CellMap, MapQuery, batch,
    traits::{LeftJoinExt, LeftSemiJoinExt, MultiLeftJoinExt},
};

const PAIRS: usize = 8;
const ITERATIONS: i64 = 3_000;

/// The tick queue is process-wide, so these `#[test]` fns (run as concurrent
/// threads within this one binary) would otherwise interfere on it. Serialize.
fn scheduler_test_serial() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// `left_join` output is `(LV, Vec<RV>)`: the left value paired with the `Vec`
/// of matching right values. With exactly one right row on the shared key "k",
/// the settled output for a pair is `(left_value, vec![right_value])`. A torn
/// survivor — new left with stale right, or vice-versa — fails the equality.
#[test]
fn concurrent_left_joins_never_settle_on_a_torn_value() {
    let _serial = scheduler_test_serial();
    let mut lefts = Vec::new();
    let mut rights = Vec::new();
    let mut outputs = Vec::new();
    for _ in 0..PAIRS {
        let l = CellMap::<String, i64>::new();
        let r = CellMap::<String, i64>::new();
        // Seed the shared join key on both sides so the left row has a match.
        l.insert("k".into(), 0);
        r.insert("k".into(), 0);
        let out = l.clone().left_join(r.clone()).materialize();
        lefts.push(l);
        rights.push(r);
        outputs.push(out);
    }

    for it in 1..=ITERATIONS {
        // Both sides of all PAIRS joins mutated in ONE batch → a single wave of
        // 2*PAIRS height-0 diff ops, wide enough to dispatch in parallel.
        batch(|| {
            #[allow(clippy::needless_range_loop)]
            for i in 0..PAIRS {
                let base = it * 1000 + i as i64;
                lefts[i].insert("k".into(), base);
                rights[i].insert("k".into(), base + 500);
            }
        });

        #[allow(clippy::needless_range_loop)]
        for i in 0..PAIRS {
            let base = it * 1000 + i as i64;
            assert_eq!(
                outputs[i].get_value(&"k".to_string()),
                Some((base, vec![base + 500])),
                "left_join settled on a torn/stale value at iteration {it}, pair {i}"
            );
        }
    }
}

/// `left_semi_join` output is just the left value `LV` (presence semantics: a
/// left row survives iff a right row shares its key — the right value never
/// appears in the output). So the exactly-correct settled output is the latest
/// left value alone. The right side is still mutated every iteration (a fresh
/// value → an `Update` diff) purely to drive the right sink's op into the wave;
/// presence stays true throughout. Here a torn survivor is a *stale left value*
/// — the right sink recomputing the output row while reading a stale left row —
/// so the assertion checks the left value, not a combined tuple.
#[test]
fn concurrent_left_semi_joins_never_settle_on_a_stale_left_value() {
    let _serial = scheduler_test_serial();
    let mut lefts = Vec::new();
    let mut rights = Vec::new();
    let mut outputs = Vec::new();
    for _ in 0..PAIRS {
        let l = CellMap::<String, i64>::new();
        let r = CellMap::<String, i64>::new();
        // Seed the shared key on both sides so the left row is kept.
        l.insert("k".into(), 0);
        r.insert("k".into(), 0);
        let out = l.clone().left_semi_join(r.clone()).materialize();
        lefts.push(l);
        rights.push(r);
        outputs.push(out);
    }

    for it in 1..=ITERATIONS {
        batch(|| {
            #[allow(clippy::needless_range_loop)]
            for i in 0..PAIRS {
                let base = it * 1000 + i as i64;
                lefts[i].insert("k".into(), base);
                rights[i].insert("k".into(), base + 500);
            }
        });

        #[allow(clippy::needless_range_loop)]
        for i in 0..PAIRS {
            let base = it * 1000 + i as i64;
            assert_eq!(
                outputs[i].get_value(&"k".to_string()),
                Some(base),
                "left_semi_join settled on a stale left value at iteration {it}, pair {i}"
            );
        }
    }
}

/// `multi_left_join_by` output is `(LV, Vec<RV>)` like `left_join`, but each
/// left item maps to a `Vec` of join keys and right items matching *any* of
/// them are unioned. Here each left item maps to the single join key "k" (the
/// map key), and the right key extractor also yields the map key "k", so the
/// join is on "k" with exactly one right match. The settled output for a pair
/// is `(left_value, vec![right_value])`; a torn survivor fails the equality.
#[test]
fn concurrent_multi_left_joins_never_settle_on_a_torn_value() {
    let _serial = scheduler_test_serial();
    let mut lefts = Vec::new();
    let mut rights = Vec::new();
    let mut outputs = Vec::new();
    for _ in 0..PAIRS {
        let l = CellMap::<String, i64>::new();
        let r = CellMap::<String, i64>::new();
        // Seed the shared join key on both sides so the left row has a match.
        l.insert("k".into(), 0);
        r.insert("k".into(), 0);
        let out = l
            .clone()
            .multi_left_join_by(
                r.clone(),
                |k: &String, _v: &i64| vec![k.clone()],
                |k: &String, _v: &i64| k.clone(),
            )
            .materialize();
        lefts.push(l);
        rights.push(r);
        outputs.push(out);
    }

    for it in 1..=ITERATIONS {
        batch(|| {
            #[allow(clippy::needless_range_loop)]
            for i in 0..PAIRS {
                let base = it * 1000 + i as i64;
                lefts[i].insert("k".into(), base);
                rights[i].insert("k".into(), base + 500);
            }
        });

        #[allow(clippy::needless_range_loop)]
        for i in 0..PAIRS {
            let base = it * 1000 + i as i64;
            assert_eq!(
                outputs[i].get_value(&"k".to_string()),
                Some((base, vec![base + 500])),
                "multi_left_join_by settled on a torn/stale value at iteration {it}, pair {i}"
            );
        }
    }
}
