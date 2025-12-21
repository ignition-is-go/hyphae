use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use crate::cell::{Cell, CellImmutable, CellMutable};
use super::Watchable;

// Completion state flags for merge (both must complete)
const SELF_COMPLETE: u8 = 0b01;
const OTHER_COMPLETE: u8 = 0b10;

pub trait MergeExt<T>: Watchable<T> {
    /// Merge with another cell - emit from whichever updates.
    fn merge<M>(&self, other: &Cell<T, M>) -> Cell<T, CellImmutable>
    where
        T: Clone + Send + Sync + 'static,
        M: Send + Sync + 'static,
        Self: Clone + Send + Sync + 'static,
    {
        let initial = self.get();
        let derived = Cell::<T, CellMutable>::new(initial);

        // Subscribe to self
        let weak1 = derived.downgrade();
        let first1 = Arc::new(AtomicBool::new(true));
        let guard1 = self.subscribe(move |value| {
            if first1.swap(false, Ordering::SeqCst) {
                return;
            }
            if let Some(d) = weak1.upgrade() {
                d.notify(value.clone());
            }
        });
        derived.own(guard1);

        // Subscribe to other
        let weak2 = derived.downgrade();
        let first2 = Arc::new(AtomicBool::new(true));
        let guard2 = other.subscribe(move |value| {
            if first2.swap(false, Ordering::SeqCst) {
                return;
            }
            if let Some(d) = weak2.upgrade() {
                d.notify(value.clone());
            }
        });
        derived.own(guard2);

        // Complete when BOTH sources complete - use atomic fetch_or to avoid TOCTOU race
        let complete_state = Arc::new(AtomicU8::new(0));

        let weak = derived.downgrade();
        let cs = complete_state.clone();
        let complete_guard1 = self.on_complete(move || {
            let prev = cs.fetch_or(SELF_COMPLETE, Ordering::SeqCst);
            if prev == OTHER_COMPLETE {
                // Other was already complete, now both are done
                if let Some(d) = weak.upgrade() {
                    d.complete();
                }
            }
        });
        derived.own(complete_guard1);

        let weak = derived.downgrade();
        let complete_guard2 = other.on_complete(move || {
            let prev = complete_state.fetch_or(OTHER_COMPLETE, Ordering::SeqCst);
            if prev == SELF_COMPLETE {
                // Self was already complete, now both are done
                if let Some(d) = weak.upgrade() {
                    d.complete();
                }
            }
        });
        derived.own(complete_guard2);

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
