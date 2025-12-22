use std::collections::HashSet;
use std::hash::Hash;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::cell::{Cell, CellImmutable, CellMutable};
use crate::signal::Signal;

use super::Watchable;

pub trait DistinctExt<T>: Watchable<T> {
    /// Filter out values that have already been emitted.
    ///
    /// Uses a HashSet to track seen values. Only emits values that
    /// haven't been seen before.
    ///
    /// # Example
    ///
    /// ```
    /// use rx3::{Cell, Mutable, DistinctExt, Watchable};
    ///
    /// let source = Cell::new(0);
    /// let distinct = source.distinct();
    ///
    /// source.set(1);
    /// source.set(2);
    /// source.set(1); // Blocked - already seen
    /// source.set(3);
    /// source.set(2); // Blocked - already seen
    /// ```
    fn distinct(&self) -> Cell<T, CellImmutable>
    where
        T: Clone + Eq + Hash + Send + Sync + 'static,
        Self: Clone + Send + Sync + 'static,
    {
        let derived = Cell::<T, CellMutable>::new(self.get());

        let weak = derived.downgrade();
        let seen: Arc<Mutex<HashSet<T>>> = Arc::new(Mutex::new(HashSet::new()));
        let first = Arc::new(AtomicBool::new(true));

        // Add initial value to seen set
        {
            let mut s = seen.lock().unwrap();
            s.insert(self.get());
        }

        let guard = self.subscribe(move |signal| {
            if let Some(d) = weak.upgrade() {
                match signal {
                    Signal::Value(value) => {
                        if first.swap(false, Ordering::SeqCst) {
                            return;
                        }
                        let mut s = seen.lock().unwrap();
                        if s.insert((**value).clone()) {
                            // Value was not in set, emit it
                            d.notify(Signal::Value(value.clone()));
                        }
                    }
                    Signal::Complete => d.notify(Signal::Complete),
                    Signal::Error(e) => d.notify(Signal::Error(e.clone())),
                }
            }
        });
        derived.own(guard);

        derived.lock()
    }
}

impl<T, W: Watchable<T>> DistinctExt<T> for W {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Mutable;
    use std::sync::atomic::AtomicU32;

    #[test]
    fn test_distinct() {
        let source = Cell::new(0);
        let distinct = source.distinct();

        let count = Arc::new(AtomicU32::new(0));
        let c = count.clone();
        let _guard = distinct.subscribe(move |signal| {
            if let Signal::Value(_) = signal {
                c.fetch_add(1, Ordering::SeqCst);
            }
        });

        assert_eq!(count.load(Ordering::SeqCst), 1); // Initial

        source.set(1);
        assert_eq!(count.load(Ordering::SeqCst), 2);

        source.set(2);
        assert_eq!(count.load(Ordering::SeqCst), 3);

        source.set(1); // Already seen
        assert_eq!(count.load(Ordering::SeqCst), 3);

        source.set(3);
        assert_eq!(count.load(Ordering::SeqCst), 4);

        source.set(2); // Already seen
        assert_eq!(count.load(Ordering::SeqCst), 4);
    }
}
