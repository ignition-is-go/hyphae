//! `zip` pairs its two inputs by arrival index using two independent lock-free
//! queues: each sink checks the *opposite* queue and, on miss, pushes its own.
//! That check-opposite-then-push-own is not atomic across the two queues. Under
//! the scheduler's wave-parallel drain, `zip`'s two input cells are distinct
//! ids at the same height, so a wide wave runs both sinks concurrently — and
//! two sinks can each observe the other's queue empty, buffer both sides, and
//! permanently mis-pair the two streams from that point on.
//!
//! Forcing the wave wide: 8 independent zip pairs driven in one `batch()` =
//! 16 height-0 ops, over PARALLEL_WAVE_THRESHOLD (8). Each iteration sets one
//! exactly-known value on each side of every pair, so correct pairing is
//! computable and checked strictly; a single mis-pair desyncs the indices and
//! every later iteration's assert fails.
#![cfg(feature = "scheduler")]

use hyphae::{Cell, Gettable, Mutable, ZipExt, batch};

const PAIRS: usize = 8;
const ITERATIONS: i64 = 3_000;

#[test]
fn concurrent_zips_never_mispair_the_two_streams() {
    // Force the parallel drain path: production defaults the wave threshold high.
    hyphae::scheduler::set_wave_threshold_for_test(4);
    let mut lefts = Vec::new();
    let mut rights = Vec::new();
    let mut outputs = Vec::new();
    for _ in 0..PAIRS {
        let l = Cell::new(0i64);
        let r = Cell::new(0i64);
        let z = l.zip(&r);
        lefts.push(l);
        rights.push(r);
        outputs.push(z);
    }

    for it in 1..=ITERATIONS {
        batch(|| {
            #[allow(clippy::needless_range_loop)]
            for i in 0..PAIRS {
                let base = it * 1000 + i as i64;
                lefts[i].set(base);
                rights[i].set(base + 500);
            }
        });

        #[allow(clippy::needless_range_loop)]
        for i in 0..PAIRS {
            let base = it * 1000 + i as i64;
            assert_eq!(
                outputs[i].get(),
                (base, base + 500),
                "zip mis-paired the streams at iteration {it}, pair {i}"
            );
        }
    }
}
