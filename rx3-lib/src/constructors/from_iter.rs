use std::{thread, time::Duration};
use crate::cell::{Cell, CellImmutable};

/// Creates a cell that emits each value from the iterator with a delay between emissions.
pub fn from_iter_with_delay<T, I>(iter: I, delay: Duration) -> Cell<T, CellImmutable>
where
    T: Clone + Send + Sync + 'static,
    I: IntoIterator<Item = T>,
    I::IntoIter: Send + 'static,
{
    let mut iter = iter.into_iter();
    let first = iter.next().expect("from_iter_with_delay requires at least one element");
    let cell = Cell::<T, CellImmutable>::derived(first, vec![]);

    let c = cell.clone();
    thread::spawn(move || {
        for value in iter {
            thread::sleep(delay);
            c.notify(value);
        }
    });

    cell
}
