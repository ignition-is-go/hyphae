mod from_iter;
mod interval;

pub use from_iter::from_iter_with_delay;
pub use interval::{IntervalTick, interval, interval_precise, interval_precise_with_elapsed};
