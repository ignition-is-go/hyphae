use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU8, Ordering},
};

use super::{CellValue, Watchable};
use crate::{
    cell::{Cell, CellImmutable, CellMutable},
    signal::Signal,
};

// Completion state flags for merge (both must complete)
const SELF_COMPLETE: u8 = 0b01;
const OTHER_COMPLETE: u8 = 0b10;

pub trait MergeExt<T>: Watchable<T> {
    /// Merge with another cell - emit from whichever updates.
    #[track_caller]
    fn merge<M>(&self, other: &Cell<T, M>) -> Cell<T, CellImmutable>
    where
        T: CellValue,
        M: Send + Sync + 'static,
        Self: Clone + Send + Sync + 'static,
    {
        let initial = self.get();
        let derived = Cell::<T, CellMutable>::new(initial);
        let derived = if let Some(name) = self.name() {
            derived.with_name(format!("{}::merge", name))
        } else {
            derived
        };

        let complete_state = Arc::new(AtomicU8::new(0));

        // Subscribe to self
        let weak1 = derived.downgrade();
        let first1 = Arc::new(AtomicBool::new(true));
        let cs1 = complete_state.clone();
        let guard1 = self.subscribe(move |signal| {
            if let Some(d) = weak1.upgrade() {
                match signal {
                    Signal::Value(_) => {
                        if first1.swap(false, Ordering::SeqCst) {
                            return;
                        }
                        d.notify(signal.clone());
                    }
                    Signal::Complete => {
                        let prev = cs1.fetch_or(SELF_COMPLETE, Ordering::SeqCst);
                        if prev == OTHER_COMPLETE {
                            d.notify(Signal::Complete);
                        }
                    }
                    Signal::Error(e) => d.notify(Signal::Error(e.clone())),
                }
            }
        });
        derived.own(guard1);

        // Subscribe to other
        let weak2 = derived.downgrade();
        let first2 = Arc::new(AtomicBool::new(true));
        let guard2 = other.subscribe(move |signal| {
            if let Some(d) = weak2.upgrade() {
                match signal {
                    Signal::Value(_) => {
                        if first2.swap(false, Ordering::SeqCst) {
                            return;
                        }
                        d.notify(signal.clone());
                    }
                    Signal::Complete => {
                        let prev = complete_state.fetch_or(OTHER_COMPLETE, Ordering::SeqCst);
                        if prev == SELF_COMPLETE {
                            d.notify(Signal::Complete);
                        }
                    }
                    Signal::Error(e) => d.notify(Signal::Error(e.clone())),
                }
            }
        });
        derived.own(guard2);

        derived.lock()
    }
}

impl<T, W: Watchable<T>> MergeExt<T> for W {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Gettable, Mutable};

    #[test]
    fn test_merge() {
        let a = Cell::new(1);
        let b = Cell::new(10);
        let merged = a.merge(&b);

        assert_eq!(merged.get(), 1); // Initial from a

        a.set(2);
        assert_eq!(merged.get(), 2);

        b.set(20);
        assert_eq!(merged.get(), 20);

        a.set(3);
        assert_eq!(merged.get(), 3);
    }
}
