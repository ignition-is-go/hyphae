//! Native (threaded) implementation of the platform primitives.

use std::{thread, time::Duration};

use rayon::prelude::*;

pub use std::time::Instant;

/// Run `f` once after `delay`, on a detached background thread.
///
/// The work is fire-and-forget: cancellation is the caller's responsibility
/// (operators use a weak upgrade / generation check inside `f`).
pub fn spawn_delayed(delay: Duration, f: impl FnOnce() + Send + 'static) {
    thread::spawn(move || {
        thread::sleep(delay);
        f();
    });
}

/// Repeatedly invoke `tick(count)` every `period`, starting one `period` after
/// the call, with `count` beginning at 1. The loop stops when `tick` returns
/// `false`.
///
/// When `precise` is set, sub-millisecond accuracy is achieved with
/// `spin_sleep` plus drift catch-up: if the thread falls behind, missed ticks
/// are folded into `count` so the emitted counter stays aligned to wall-clock.
pub fn spawn_interval(
    period: Duration,
    precise: bool,
    mut tick: impl FnMut(u64) -> bool + Send + 'static,
) {
    thread::spawn(move || {
        let mut count: u64 = 0;
        let mut next_tick = Instant::now() + period;

        loop {
            if precise {
                let now = Instant::now();
                if next_tick > now {
                    spin_sleep::sleep(next_tick - now);
                }
            } else {
                thread::sleep(period);
            }

            count += 1;
            if !tick(count) {
                break;
            }

            if precise {
                next_tick += period;
                // If we've fallen behind, skip missed ticks to avoid drift.
                let now = Instant::now();
                if next_tick < now {
                    let missed = ((now - next_tick).as_nanos() / period.as_nanos()) as u64;
                    count += missed;
                    next_tick = now + period;
                }
            }
        }
    });
}

/// Fan out `f` across `items` in parallel using rayon.
pub fn par_for_each<T: Sync>(items: &[T], f: impl Fn(&T) + Send + Sync) {
    items.par_iter().for_each(f);
}
