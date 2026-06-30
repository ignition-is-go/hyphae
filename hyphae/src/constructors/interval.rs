use std::time::Duration;

use crate::{
    cell::{Cell, CellImmutable, CellMutable},
    platform::{self, Instant},
    signal::Signal,
    source::Source,
};

/// Creates a cell that emits 0, 1, 2, ... on the given interval.
///
/// The timer automatically stops when the cell is dropped.
///
/// For intervals under 10ms, consider using [`interval_precise`] for better
/// accuracy (native only — on wasm both map to `setInterval`).
///
/// **Performance note:** if you only need notification (not the current tick
/// count via `.get()`), prefer [`interval_source`] — it skips the per-tick
/// `ArcSwap` drop on the stored value.
pub fn interval(duration: Duration) -> Cell<u64, CellImmutable> {
    let cell = Cell::<u64, CellMutable>::new(0);

    // Use weak ref so the timer doesn't keep cell alive
    let weak = cell.downgrade();
    platform::spawn_interval(duration, false, move |count| {
        // Exit when cell is dropped
        let Some(c) = weak.upgrade() else {
            return false;
        };
        c.notify(Signal::value(count));
        true
    });

    cell.lock()
}

/// Like [`interval`] but returns a [`Source`] instead of a [`Cell`].
///
/// `Source` skips the value `ArcSwap` store on every emission — useful when
/// the interval is high-rate and consumers only need notification (not
/// `.get()` of the current tick count). Most existing callers that just
/// `subscribe` or use the cell as a notifier in `.sample` can migrate to
/// this variant for a per-tick saving.
pub fn interval_source(duration: Duration) -> Source<u64> {
    let source = Source::<u64>::new();

    let weak = source.downgrade();
    platform::spawn_interval(duration, false, move |count| {
        let Some(s) = weak.upgrade() else {
            return false;
        };
        s.emit(count);
        true
    });

    source
}

/// Creates a high-precision interval cell that emits 0, 1, 2, ... at the given frequency.
///
/// On native targets this uses spin-sleeping for sub-millisecond precision,
/// suitable for high-frequency applications like 240Hz sync clocks. On wasm
/// there is no sub-millisecond timer, so this behaves like [`interval`].
///
/// **Performance note:** prefer [`interval_precise_source`] when consumers
/// only need notification.
///
/// # Arguments
///
/// * `duration` - The interval between ticks
///
/// # Example
///
/// ```ignore
/// use std::time::Duration;
/// use hyphae::{interval_precise, Watchable};
///
/// // 240 Hz = ~4.16ms interval
/// let clock = interval_precise(Duration::from_secs_f64(1.0 / 240.0));
///
/// let _guard = clock.subscribe(|signal| {
///     if let hyphae::Signal::Value(tick) = signal {
///         println!("Tick: {}", tick);
///     }
/// });
/// ```
pub fn interval_precise(duration: Duration) -> Cell<u64, CellImmutable> {
    let cell = Cell::<u64, CellMutable>::new(0);

    let weak = cell.downgrade();
    platform::spawn_interval(duration, true, move |count| {
        let Some(c) = weak.upgrade() else {
            return false;
        };
        c.notify(Signal::value(count));
        true
    });

    cell.lock()
}

/// Like [`interval_precise`] but returns a [`Source`] instead of a [`Cell`].
pub fn interval_precise_source(duration: Duration) -> Source<u64> {
    let source = Source::<u64>::new();

    let weak = source.downgrade();
    platform::spawn_interval(duration, true, move |count| {
        let Some(s) = weak.upgrade() else {
            return false;
        };
        s.emit(count);
        true
    });

    source
}

/// Tick data emitted by [`interval_precise_with_elapsed`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IntervalTick {
    /// Monotonically increasing tick count (starts at 1)
    pub tick: u64,
    /// Time elapsed since the interval started
    pub elapsed: Duration,
}

impl Default for IntervalTick {
    fn default() -> Self {
        Self {
            tick: 0,
            elapsed: Duration::ZERO,
        }
    }
}

/// Creates a high-precision interval cell that emits tick count and elapsed time.
///
/// Similar to [`interval_precise`] but includes elapsed time for each tick,
/// useful for time-based calculations.
///
/// **Performance note:** prefer [`interval_precise_with_elapsed_source`] when
/// consumers only need notification.
pub fn interval_precise_with_elapsed(duration: Duration) -> Cell<IntervalTick, CellImmutable> {
    let cell = Cell::<IntervalTick, CellMutable>::new(IntervalTick::default());

    let weak = cell.downgrade();
    let start = Instant::now();
    platform::spawn_interval(duration, true, move |count| {
        let Some(c) = weak.upgrade() else {
            return false;
        };
        c.notify(Signal::value(IntervalTick {
            tick: count,
            elapsed: start.elapsed(),
        }));
        true
    });

    cell.lock()
}

/// Like [`interval_precise_with_elapsed`] but returns a [`Source`] instead of a [`Cell`].
pub fn interval_precise_with_elapsed_source(duration: Duration) -> Source<IntervalTick> {
    let source = Source::<IntervalTick>::new();

    let weak = source.downgrade();
    let start = Instant::now();
    platform::spawn_interval(duration, true, move |count| {
        let Some(s) = weak.upgrade() else {
            return false;
        };
        s.emit(IntervalTick {
            tick: count,
            elapsed: start.elapsed(),
        });
        true
    });

    source
}
