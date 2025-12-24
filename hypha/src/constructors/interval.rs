use std::{thread, time::Duration};

use crate::{
    cell::{Cell, CellImmutable, CellMutable},
    signal::Signal,
};

/// Creates a cell that emits 0, 1, 2, ... on the given interval.
///
/// The thread automatically stops when the cell is dropped.
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
