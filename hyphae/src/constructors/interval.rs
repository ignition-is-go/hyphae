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
/// **Performance note:** if you only need notification (not the current tick
/// count via `.get()`), prefer [`interval_source`] — it skips the per-tick
/// `ArcSwap` drop on the stored value.
#[must_use]
pub fn interval(duration: Duration) -> Cell<u64, CellImmutable> {
    let cell = Cell::<u64, CellMutable>::new(0);

    // Use weak ref so the timer doesn't keep cell alive
    let weak = cell.downgrade();
    platform::spawn_interval(duration, move |count| {
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
#[must_use]
pub fn interval_source(duration: Duration) -> Source<u64> {
    let source = Source::<u64>::new();

    let weak = source.downgrade();
    platform::spawn_interval(duration, move |count| {
        let Some(s) = weak.upgrade() else {
            return false;
        };
        s.emit(count);
        true
    });

    source
}

/// Tick data emitted by [`interval_with_elapsed`].
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

/// Creates an interval cell that emits the tick count and the time elapsed
/// since the interval started.
///
/// Like [`interval`] but includes elapsed time for each tick, useful for
/// time-based calculations.
///
/// **Performance note:** prefer [`interval_with_elapsed_source`] when
/// consumers only need notification.
#[must_use]
pub fn interval_with_elapsed(duration: Duration) -> Cell<IntervalTick, CellImmutable> {
    let cell = Cell::<IntervalTick, CellMutable>::new(IntervalTick::default());

    let weak = cell.downgrade();
    let start = Instant::now();
    platform::spawn_interval(duration, move |count| {
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

/// Like [`interval_with_elapsed`] but returns a [`Source`] instead of a [`Cell`].
#[must_use]
pub fn interval_with_elapsed_source(duration: Duration) -> Source<IntervalTick> {
    let source = Source::<IntervalTick>::new();

    let weak = source.downgrade();
    let start = Instant::now();
    platform::spawn_interval(duration, move |count| {
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
