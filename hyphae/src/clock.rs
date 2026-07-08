//! Injectable time and tick abstraction — the seam for frame-locked propagation.
//!
//! hyphae's default timing is *decentralized*: every time-based operator
//! (`delay`, `debounce`, `interval`, …) spawns its own timer off
//! [`crate::platform`], and monotonic time is read directly from
//! `platform::Instant`. That is fine for eager per-`set` propagation, but it
//! gives no place to (a) drive propagation from a single frame boundary or
//! (b) discipline the timebase to an external reference (PTP / IEEE-1588).
//!
//! This module introduces two small, pluggable traits:
//!
//! - [`Clock`] — a source of monotonic (and optionally *absolute*) nanoseconds.
//!   The default [`MonotonicClock`] wraps `platform::now_nanos()` and is a pure
//!   pass-through with no behavior change. A future `DisciplinedClock` will add
//!   a smoothed PTP offset via [`Clock::to_absolute_nanos`].
//! - [`TickSource`] — a driver that fires a callback once per frame/tick, each
//!   carrying a [`Tick`] (`index` + frozen frame time). The default
//!   [`IntervalTickSource`] wraps `platform::spawn_interval`. A future
//!   `PllTickSource` will slew this grid to phase-lock to PTP.
//!
//! Nothing here changes hyphae's synchronous default. The traits are consumed
//! only by the opt-in [`crate::scheduler`] (both gated behind the `scheduler`
//! feature); the clock is intended to be **handle-scoped**, not a process
//! global, so multiple engines can run on independent timebases.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

/// A source of monotonic — and optionally absolute — time in nanoseconds.
///
/// Implementations must guarantee [`now_nanos`](Clock::now_nanos) is
/// non-decreasing on a given thread of observation. The absolute value returned
/// by `now_nanos` is meaningless across clocks; only *differences* are.
pub trait Clock: Send + Sync {
    /// Monotonic nanoseconds since this clock's fixed (but arbitrary) base.
    fn now_nanos(&self) -> u64;

    /// Map a monotonic reading to an *absolute* timebase (e.g. PTP wall time),
    /// if this clock is disciplined to one. `None` for a purely monotonic
    /// clock. Frame-lock consumers use this to place frame N at the same
    /// absolute instant on every machine.
    fn to_absolute_nanos(&self, _now_nanos: u64) -> Option<u64> {
        None
    }
}

/// One propagation tick — a single frame boundary.
///
/// Carries the frame `index` and the frame time *frozen* at the boundary, so
/// every pure computation within the tick reads one consistent timestamp
/// (a prerequisite for deterministic, replicable frames).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tick {
    /// Monotonically increasing frame counter, starting at 1.
    pub index: u64,
    /// Monotonic clock reading at the frame boundary (from [`Clock::now_nanos`]).
    pub clock_nanos: u64,
    /// Absolute (e.g. PTP) time at the boundary, if the clock is disciplined.
    pub absolute_nanos: Option<u64>,
}

/// Cancels tick delivery when dropped.
///
/// Holding the guard keeps the underlying timer firing; dropping it stops the
/// callback registered by [`TickSource::on_tick`] (the timer thread/task exits
/// on its next wake). Use [`TickGuard::leak`] to keep ticking for the process
/// lifetime without holding the guard.
#[must_use = "dropping the TickGuard immediately stops tick delivery"]
pub struct TickGuard {
    alive: Arc<AtomicBool>,
}

impl TickGuard {
    /// Detach the guard: ticks continue for the process lifetime.
    pub fn leak(self) {
        std::mem::forget(self);
    }
}

impl Drop for TickGuard {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Relaxed);
    }
}

/// A driver that fires a callback once per tick (frame).
///
/// The callback runs on the tick source's own thread/task; the returned
/// [`TickGuard`] stops delivery on drop.
pub trait TickSource: Send + Sync {
    /// Register `f` to run once per tick. Returns a guard that unregisters on
    /// drop.
    fn on_tick(&self, f: Box<dyn FnMut(Tick) + Send>) -> TickGuard;
}

/// Default [`Clock`]: monotonic nanoseconds from `platform::now_nanos()`.
///
/// A pure pass-through — no discipline, no absolute timebase. This is what the
/// scheduler uses until an application injects a PTP-disciplined clock.
#[derive(Debug, Clone, Default)]
pub struct MonotonicClock {
    _private: (),
}

impl MonotonicClock {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Clock for MonotonicClock {
    fn now_nanos(&self) -> u64 {
        crate::platform::now_nanos()
    }
}

/// Default [`TickSource`]: a fixed-period interval over `platform::spawn_interval`.
///
/// Each `on_tick` registration spawns one interval timer (native: a thread with
/// `spin_sleep` when `precise`; wasm: a `setTimeout` loop, `precise` ignored).
/// Ticks are stamped with the source's [`Clock`] — [`MonotonicClock`] by
/// default, or an injected disciplined clock via [`with_clock`](Self::with_clock).
pub struct IntervalTickSource {
    period: Duration,
    precise: bool,
    clock: Arc<dyn Clock>,
}

impl IntervalTickSource {
    /// A tick source firing every `period`, stamped with a [`MonotonicClock`].
    pub fn new(period: Duration) -> Self {
        Self {
            period,
            precise: false,
            clock: Arc::new(MonotonicClock::new()),
        }
    }

    /// Enable sub-millisecond precision (native only; ignored on wasm).
    pub fn precise(mut self) -> Self {
        self.precise = true;
        self
    }

    /// Stamp ticks with an injected clock (e.g. a PTP-disciplined one) instead
    /// of the default monotonic clock.
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }
}

impl TickSource for IntervalTickSource {
    fn on_tick(&self, mut f: Box<dyn FnMut(Tick) + Send>) -> TickGuard {
        let alive = Arc::new(AtomicBool::new(true));
        let alive_timer = alive.clone();
        let clock = self.clock.clone();
        crate::platform::spawn_interval(self.period, self.precise, move |count| {
            // Stop the timer once the guard is gone.
            if !alive_timer.load(Ordering::Relaxed) {
                return false;
            }
            let clock_nanos = clock.now_nanos();
            f(Tick {
                index: count,
                clock_nanos,
                absolute_nanos: clock.to_absolute_nanos(clock_nanos),
            });
            true
        });
        TickGuard { alive }
    }
}
