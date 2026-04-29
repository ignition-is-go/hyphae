mod from_iter;
#[cfg(not(target_arch = "wasm32"))]
mod interval;

pub use from_iter::from_iter_with_delay;
#[cfg(not(target_arch = "wasm32"))]
pub use interval::{
    IntervalTick, interval, interval_precise, interval_precise_source,
    interval_precise_with_elapsed, interval_precise_with_elapsed_source, interval_source,
};
