//! CellMap analogue of the join.rs / join_vec.rs torn-value repro, under the
//! scheduler's wave-parallel drain.
//!
//! A two-input CellMap join (`inner_join`, `left_join`, `multi_left_join`) has
//! a left sink and a right sink, each driven by a distinct height-0 root
//! (`left.diffs_cell` / `right.diffs_cell`). Each sink reads the *sibling*
//! side's rows to build the combined output row. Under wave-parallel draining
//! (a wave of >= PARALLEL_WAVE_THRESHOLD same-height ops runs on rayon), the
//! two sinks can run at the literal same instant on different threads; if the
//! emit is not ordered consistently with the sibling read, two concurrent
//! updates to one output key can last-write-wins a stale combined value.
//!
//! To force a genuinely parallel wave we drive PAIRS independent joins in one
//! `batch()`: 8 pairs = 16 height-0 diff-cell ops, comfortably over the
//! threshold, so each join's own left+right ops run concurrently. Every
//! iteration sets a fresh, exactly-known value on both sides of every pair, so
//! the correct settled output is computable and checked strictly — a torn
//! survivor (new value on one side, stale on the other) fails the assert.
#![cfg(feature = "scheduler")]

use hyphae::{CellMap, MapQuery, batch, traits::InnerJoinExt};

const PAIRS: usize = 8;
const ITERATIONS: i64 = 3_000;

#[test]
fn concurrent_inner_joins_never_settle_on_a_torn_value() {
    let mut lefts = Vec::new();
    let mut rights = Vec::new();
    let mut outputs = Vec::new();
    for _ in 0..PAIRS {
        let l = CellMap::<String, i64>::new();
        let r = CellMap::<String, i64>::new();
        // Seed the shared join key on both sides so the inner join emits.
        l.insert("k".into(), 0);
        r.insert("k".into(), 0);
        let out = l.clone().inner_join(r.clone()).materialize();
        lefts.push(l);
        rights.push(r);
        outputs.push(out);
    }

    for it in 1..=ITERATIONS {
        // Both sides of all PAIRS joins mutated in ONE batch → a single wave of
        // 2*PAIRS height-0 diff ops, wide enough to dispatch in parallel.
        batch(|| {
            // Indexes three parallel Vecs (lefts/rights here, outputs below) by
            // the same counter plus uses `i` in arithmetic — `enumerate()` over
            // any single one doesn't fit this shape.
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
                Some((base, base + 500)),
                "inner_join settled on a torn/stale value at iteration {it}, pair {i}"
            );
        }
    }
}
