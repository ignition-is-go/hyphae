use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use parking_lot::Mutex;

use super::{CellValue, Watchable};
use crate::{
    cell::{Cell, CellImmutable, CellMutable},
    signal::Signal,
    traits::Mutable,
};

/// Combines a vector of cells into a single cell that emits `Vec<T>`.
///
/// The resulting cell emits whenever any input cell changes.
/// Completes when all input cells complete.
/// Errors immediately if any input cell errors.
///
/// # Example
/// ```
/// use hyphae::{Cell, Mutable, Gettable, join_vec};
///
/// let a = Cell::new(1);
/// let b = Cell::new(2);
/// let c = Cell::new(3);
///
/// let combined = join_vec(vec![a.clone().lock(), b.lock(), c.lock()]);
/// assert_eq!(combined.get(), vec![1, 2, 3]);
///
/// a.set(10);
/// assert_eq!(combined.get(), vec![10, 2, 3]);
/// ```
#[track_caller]
pub fn join_vec<T, W>(cells: Vec<W>) -> Cell<Vec<T>, CellImmutable>
where
    T: CellValue,
    W: Watchable<T> + Clone + Send + Sync + 'static,
{
    let caller = std::panic::Location::caller();
    if cells.is_empty() {
        let derived = Cell::<Vec<T>, CellMutable>::new(vec![]);
        derived.complete();
        return derived
            .with_name(format!(
                "join_vec@{}:{}:{}[0]",
                caller.file(),
                caller.line(),
                caller.column()
            ))
            .lock();
    }

    // Get initial values
    let initial: Vec<T> = cells.iter().map(|c| c.get()).collect();
    // Shared "last known values", one slot per cell, held through the ENTIRE
    // update-then-notify sequence below (not just the read that builds the
    // combined vec). This matters only under the `scheduler` feature's
    // wave-parallel draining, where multiple same-height cells can now
    // notify at the literal same instant on different threads: each cell's
    // callback used to re-read every cell's `.get()` independently, so
    // whichever notify's *push* into the scheduler's coalescing slot landed
    // last could carry a stale peek at a sibling that hadn't updated yet,
    // leaving a torn combined vec as the survivor (same class of bug
    // confirmed on `join`'s two-cell version by repro). Holding this lock
    // across the notify call, not just the read, guarantees whichever
    // cell's push actually lands last also reflects the freshest state of
    // every cell — no sibling update can land in the gap between "I read
    // the combined vec" and "I pushed it".
    let latest: Arc<Mutex<Vec<T>>> = Arc::new(Mutex::new(initial.clone()));
    let derived = Cell::<Vec<T>, CellMutable>::new(initial);
    let join_name = if let Some(name) = cells.first().and_then(|c| c.name()) {
        format!(
            "{}::join_vec@{}:{}:{}[{}]",
            name,
            caller.file(),
            caller.line(),
            caller.column(),
            num_cells_from(&cells)
        )
    } else {
        format!(
            "join_vec@{}:{}:{}[{}]",
            caller.file(),
            caller.line(),
            caller.column(),
            num_cells_from(&cells)
        )
    };
    let derived = derived.with_name(join_name);

    let num_cells = cells.len();
    let complete_count = Arc::new(AtomicUsize::new(0));

    // Subscribe to each cell
    for (i, cell) in cells.iter().enumerate() {
        let weak = derived.downgrade();
        let first = Arc::new(AtomicBool::new(true));
        let cc = complete_count.clone();
        let nc = num_cells;
        let latest = latest.clone();

        let guard = cell.subscribe(move |signal| {
            if let Some(d) = weak.upgrade() {
                match signal {
                    Signal::Value(v) => {
                        // Skip first emission (initial value already set)
                        if first.swap(false, Ordering::SeqCst) {
                            return;
                        }
                        let mut guard = latest.lock();
                        guard[i] = v.as_ref().clone();
                        d.notify(Signal::value(guard.clone()));
                    }
                    Signal::Complete => {
                        let prev = cc.fetch_add(1, Ordering::SeqCst);
                        if prev + 1 == nc {
                            // All cells have completed
                            d.notify(Signal::Complete);
                        }
                    }
                    Signal::Error(e) => {
                        // Error from any cell propagates immediately
                        d.notify(Signal::Error(e.clone()));
                    }
                }
            }
        });
        derived.own(guard);
    }

    derived.lock()
}

fn num_cells_from<T, W>(cells: &[W]) -> usize
where
    T: CellValue,
    W: Watchable<T> + Clone + Send + Sync + 'static,
{
    cells.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Mutable, traits::Gettable};

    #[test]
    fn test_join_vec_empty() {
        let combined: Cell<Vec<i32>, CellImmutable> =
            join_vec::<i32, Cell<i32, CellImmutable>>(vec![]);
        assert_eq!(combined.get(), Vec::<i32>::new());
        assert!(combined.is_complete());
    }

    #[test]
    fn test_join_vec_single() {
        let a = Cell::new(42);
        let a_locked = a.clone().lock();
        let combined = join_vec(vec![a_locked]);
        assert_eq!(combined.get(), vec![42]);

        a.set(100);
        assert_eq!(combined.get(), vec![100]);
    }

    #[test]
    fn test_join_vec_multiple() {
        let a = Cell::new(1);
        let b = Cell::new(2);
        let c = Cell::new(3);

        let combined = join_vec(vec![a.clone().lock(), b.clone().lock(), c.clone().lock()]);
        assert_eq!(combined.get(), vec![1, 2, 3]);

        a.set(10);
        assert_eq!(combined.get(), vec![10, 2, 3]);

        b.set(20);
        assert_eq!(combined.get(), vec![10, 20, 3]);

        c.set(30);
        assert_eq!(combined.get(), vec![10, 20, 30]);
    }

    #[test]
    fn test_join_vec_completion() {
        let a = Cell::new(1);
        let b = Cell::new(2);

        let combined = join_vec(vec![a.clone().lock(), b.clone().lock()]);
        assert!(!combined.is_complete());

        a.complete();
        assert!(!combined.is_complete());

        b.complete();
        assert!(combined.is_complete());
    }

    #[test]
    fn test_join_vec_subscription() {
        use std::sync::atomic::AtomicI32;

        let a = Cell::new(1);
        let b = Cell::new(2);

        let combined = join_vec(vec![a.clone().lock(), b.clone().lock()]);

        let count = Arc::new(AtomicI32::new(0));
        let count_clone = count.clone();

        let _guard = combined.subscribe(move |signal| {
            if let Signal::Value(_) = signal {
                count_clone.fetch_add(1, Ordering::SeqCst);
            }
        });

        // Initial subscription triggers once
        assert_eq!(count.load(Ordering::SeqCst), 1);

        a.set(10);
        assert_eq!(count.load(Ordering::SeqCst), 2);

        b.set(20);
        assert_eq!(count.load(Ordering::SeqCst), 3);
    }
}
