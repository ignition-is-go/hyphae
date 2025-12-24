use std::{thread, time::Duration};

use crate::{
    cell::{Cell, CellImmutable, CellMutable},
    signal::Signal,
};

/// Creates a cell that emits each value from the iterator with a delay between emissions.
///
/// Returns `None` if the iterator is empty.
/// The thread automatically stops when the cell is dropped.
pub fn from_iter_with_delay<T, I>(iter: I, delay: Duration) -> Option<Cell<T, CellImmutable>>
where
    T: Clone + Send + Sync + 'static,
    I: IntoIterator<Item = T>,
    I::IntoIter: Send + 'static,
{
    let mut iter = iter.into_iter();
    let first = iter.next()?;
    let cell = Cell::<T, CellMutable>::new(first);

    // Use weak ref so thread doesn't keep cell alive
    let weak = cell.downgrade();
    thread::spawn(move || {
        for value in iter {
            thread::sleep(delay);
            // Exit when cell is dropped
            let Some(c) = weak.upgrade() else { break };
            c.notify(Signal::value(value));
        }
        // Complete when iterator exhausted
        if let Some(c) = weak.upgrade() {
            c.notify(Signal::Complete);
        }
    });

    Some(cell.lock())
}
