//! WebAssembly implementation of the platform primitives.
//!
//! Timers are driven as futures on the browser event loop:
//! [`wasm_bindgen_futures::spawn_local`] owns each task and
//! [`gloo_timers::future::sleep`] (backed by `setTimeout`) provides the delay.
//! Because the executor owns and drops the future when it completes, there is
//! no per-tick closure leak and no closure is ever dropped from inside its own
//! callback — the two hazards of the raw `setInterval` callback API.
//!
//! Monotonic time comes from `performance.now()` (through `web-time`). There
//! are no OS threads, so the `precise` flag on [`spawn_interval`] is ignored
//! (`setTimeout` is millisecond-resolution and subject to browser throttling)
//! and [`par_for_each`] runs sequentially.

use std::time::Duration;

use gloo_timers::future::sleep;
use wasm_bindgen_futures::spawn_local;
pub use web_time::Instant;

/// Run `f` once after `delay`, as a task on the browser event loop.
///
/// The task is owned by the executor and freed once `f` runs — no leak.
/// Cancellation is the caller's responsibility (operators use a weak upgrade /
/// generation check inside `f`).
pub fn spawn_delayed(delay: Duration, f: impl FnOnce() + Send + 'static) {
    spawn_local(async move {
        sleep(delay).await;
        f();
    });
}

/// Repeatedly invoke `tick(count)` every `period`, with `count` beginning at 1.
/// The task ends (and stops re-arming the timer) when `tick` returns `false`.
///
/// `precise` is ignored on wasm — there is no sub-millisecond timer available
/// in the browser.
pub fn spawn_interval(
    period: Duration,
    _precise: bool,
    mut tick: impl FnMut(u64) -> bool + Send + 'static,
) {
    spawn_local(async move {
        let mut count: u64 = 0;
        loop {
            sleep(period).await;
            count += 1;
            if !tick(count) {
                break;
            }
        }
    });
}

/// Fan out `f` across `items`. wasm is single-threaded, so this runs
/// sequentially — the API matches the native rayon fan-out.
pub fn par_for_each<T: Sync>(items: &[T], f: impl Fn(&T) + Send + Sync) {
    items.iter().for_each(f);
}
