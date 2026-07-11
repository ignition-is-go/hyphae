//! Adversarial stress for `switch_map`'s stale-inner-wins race — the
//! lowest-confidence of the wave-parallel fixes because the window (an old
//! inner reading the generation, passing its staleness guard, then having its
//! now-stale value win the output's coalescing slot over the switched-in inner)
//! is narrow. `parallelism_combiners.rs` covers it at width 8 / 3000 iters; this
//! widens the wave hard (so rayon genuinely runs many old-inner-vs-switch races
//! at once) and runs an order of magnitude longer, to give a surviving hole the
//! maximum chance to surface.
#![cfg(feature = "scheduler")]
#![allow(clippy::needless_range_loop)]

use hyphae::{Cell, Gettable, Mutable, SwitchMapExt, batch};

// Wide enough that each batch is a 2*WIDTH height-0 wave dispatched across
// rayon, so many units' old-inner emissions race their selector's gen-bump
// concurrently. Long enough to hammer the narrow window.
const WIDTH: usize = 64;
const ITERATIONS: i64 = 8_000;

#[test]
fn switch_map_latest_inner_always_wins_under_wide_prolonged_contention() {
    for it in 1..=ITERATIONS {
        let mut sels = Vec::with_capacity(WIDTH);
        let mut old_srcs = Vec::with_capacity(WIDTH);
        let mut results = Vec::with_capacity(WIDTH);

        for i in 0..WIDTH {
            let sel = Cell::new(0i64); // starts on inner 0 (the old inner)
            let old_src = Cell::new(0i64);
            let new_val = it * 1000 + i as i64 + 500;
            let new_src = Cell::new(new_val);
            // Both inners are bare height-0 sources — same height as the
            // selector, so selector-set and old-inner-set collide in one wave.
            let old = old_src.clone().lock();
            let new = new_src.clone().lock();
            let result = sel.switch_map(move |&k| if k == 0 { old.clone() } else { new.clone() });
            sels.push(sel);
            old_srcs.push(old_src);
            results.push(result);
        }

        // In one batch, per unit: fire the OLD inner (a stale value) AND switch
        // to the new inner. The old-inner emission races the generation bump.
        batch(|| {
            for i in 0..WIDTH {
                old_srcs[i].set(it * 1000 + i as i64); // stale
                sels[i].set(1); // switch
            }
        });

        for i in 0..WIDTH {
            let expected = it * 1000 + i as i64 + 500;
            assert_eq!(
                results[i].get(),
                expected,
                "switch_map settled on a STALE old-inner value at iteration {it}, unit {i}"
            );
        }
    }
}
