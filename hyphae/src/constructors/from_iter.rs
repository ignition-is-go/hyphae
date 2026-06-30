use std::time::Duration;

use crate::{
    cell::{Cell, CellImmutable, CellMutable},
    platform,
    signal::Signal,
    traits::CellValue,
};

/// Creates a cell that emits each value from the iterator with a delay between emissions.
///
/// Returns `None` if the iterator is empty.
/// The thread automatically stops when the cell is dropped.
#[track_caller]
pub fn from_iter_with_delay<T, I>(iter: I, delay: Duration) -> Option<Cell<T, CellImmutable>>
where
    T: CellValue,
    I: IntoIterator<Item = T>,
    I::IntoIter: Send + 'static,
{
    let mut iter = iter.into_iter();
    let first = iter.next()?;
    let cell = Cell::<T, CellMutable>::new(first);

    // Use weak ref so the timer doesn't keep the cell alive
    let weak = cell.downgrade();
    platform::spawn_interval(delay, false, move |_count| match iter.next() {
        Some(value) => {
            // Exit when cell is dropped
            let Some(c) = weak.upgrade() else {
                return false;
            };
            c.notify(Signal::value(value));
            true
        }
        None => {
            // Complete when iterator exhausted
            if let Some(c) = weak.upgrade() {
                c.notify(Signal::Complete);
            }
            false
        }
    });

    Some(cell.lock())
}
