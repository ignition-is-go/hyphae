use std::{
    thread,
    time::{Duration, Instant},
};

use crate::{
    cell::{Cell, CellImmutable, CellMutable},
    signal::Signal,
};

/// Creates a cell that emits 0, 1, 2, ... on the given interval.
///
/// The thread automatically stops when the cell is dropped.
///
/// For intervals under 10ms, consider using [`interval_precise`] for better accuracy.
pub fn interval(duration: Duration) -> Cell<u64, CellImmutable> {
    let cell = Cell::<u64, CellMutable>::new(0);

    // Use weak ref so thread doesn't keep cell alive
    let weak = cell.downgrade();
    thread::spawn(move || {
        let mut count: u64 = 0;
        loop {
            thread::sleep(duration);
            count += 1;
            // Exit when cell is dropped
            let Some(c) = weak.upgrade() else { break };
            c.notify(Signal::value(count));
        }
    });

    cell.lock()
}

/// Creates a high-precision interval cell that emits 0, 1, 2, ... at the given frequency.
///
/// Uses spin-sleeping for sub-millisecond precision, suitable for high-frequency
/// applications like 240Hz sync clocks.
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
    thread::spawn(move || {
        let mut count: u64 = 0;
        let mut next_tick = Instant::now() + duration;

        loop {
            // High-precision sleep using spin_sleep
            let now = Instant::now();
            if next_tick > now {
                spin_sleep::sleep(next_tick - now);
            }

            count += 1;

            // Exit when cell is dropped
            let Some(c) = weak.upgrade() else { break };
            c.notify(Signal::value(count));

            // Schedule next tick
            next_tick += duration;

            // If we've fallen behind, catch up (avoid drift accumulation)
            let now = Instant::now();
            if next_tick < now {
                // Skip missed ticks, reset to next future tick
                let missed = ((now - next_tick).as_nanos() / duration.as_nanos()) as u64;
                count += missed;
                next_tick = now + duration;
            }
        }
    });

    cell.lock()
}

/// Tick data emitted by [`interval_precise_with_elapsed`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IntervalTick {
    /// Monotonically increasing tick count (starts at 1)
    pub tick: u64,
    /// Time elapsed since the interval started
    pub elapsed: Duration,
}

/// Creates a high-precision interval cell that emits tick count and elapsed time.
///
/// Similar to [`interval_precise`] but includes elapsed time for each tick,
/// useful for time-based calculations.
pub fn interval_precise_with_elapsed(duration: Duration) -> Cell<IntervalTick, CellImmutable> {
    let cell = Cell::<IntervalTick, CellMutable>::new(IntervalTick {
        tick: 0,
        elapsed: Duration::ZERO,
    });

    let weak = cell.downgrade();
    thread::spawn(move || {
        let start = Instant::now();
        let mut count: u64 = 0;
        let mut next_tick = Instant::now() + duration;

        loop {
            // High-precision sleep
            let now = Instant::now();
            if next_tick > now {
                spin_sleep::sleep(next_tick - now);
            }

            count += 1;

            // Exit when cell is dropped
            let Some(c) = weak.upgrade() else { break };
            c.notify(Signal::value(IntervalTick {
                tick: count,
                elapsed: start.elapsed(),
            }));

            // Schedule next tick
            next_tick += duration;

            // Catch up if behind
            let now = Instant::now();
            if next_tick < now {
                let missed = ((now - next_tick).as_nanos() / duration.as_nanos()) as u64;
                count += missed;
                next_tick = now + duration;
            }
        }
    });

    cell.lock()
}
