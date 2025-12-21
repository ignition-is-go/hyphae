use std::{thread, time::Duration};
use crate::cell::{Cell, CellImmutable};

/// Creates a cell that emits 0, 1, 2, ... on the given interval.
pub fn interval(duration: Duration) -> Cell<u64, CellImmutable> {
    let cell = Cell::<u64, CellImmutable>::derived(0, vec![]);

    let c = cell.clone();
    thread::spawn(move || {
        let mut count: u64 = 0;
        loop {
            thread::sleep(duration);
            count += 1;
            c.notify(count);
        }
    });

    cell
}
