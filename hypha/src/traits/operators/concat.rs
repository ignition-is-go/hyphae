use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::cell::{Cell, CellImmutable, CellMutable};
use crate::signal::Signal;

use super::Watchable;

pub trait ConcatExt<T>: Watchable<T> {
    /// Sequential stream composition - subscribe to second source after first completes.
    ///
    /// Values from the first source are emitted until it completes, then values
    /// from the second source are emitted.
    ///
    /// # Example
    ///
    /// ```
    /// use hypha::{Cell, Mutable, Gettable, ConcatExt};
    ///
    /// let first = Cell::new(1);
    /// let second = Cell::new(100);
    /// let combined = first.concat(&second);
    ///
    /// first.set(2);   // Emits 2
    /// first.set(3);   // Emits 3
    /// first.complete(); // Switches to second
    /// second.set(200); // Emits 200
    /// ```
    fn concat<W2>(&self, other: &W2) -> Cell<T, CellImmutable>
    where
        T: Clone + Send + Sync + 'static,
        W2: Watchable<T> + Clone + Send + Sync + 'static,
        Self: Clone + Send + Sync + 'static,
    {
        let derived = Cell::<T, CellMutable>::new(self.get());

        let weak = derived.downgrade();
        let first_done = Arc::new(AtomicBool::new(false));
        let first_skip = Arc::new(AtomicBool::new(true));
        let other = other.clone();

        // Subscribe to second source when first completes
        let first_done2 = first_done.clone();
        let guard1 = self.subscribe(move |signal| {
            if let Some(d) = weak.upgrade() {
                match signal {
                    Signal::Value(value) => {
                        if first_skip.swap(false, Ordering::SeqCst) {
                            return;
                        }
                        d.notify(Signal::Value(value.clone()));
                    }
                    Signal::Complete => {
                        first_done2.store(true, Ordering::SeqCst);
                        // Now subscribe to second source
                        let weak2 = d.downgrade();
                        let second_skip = Arc::new(AtomicBool::new(true));
                        let guard2 = other.subscribe(move |signal| {
                            if let Some(d2) = weak2.upgrade() {
                                match signal {
                                    Signal::Value(value) => {
                                        if second_skip.swap(false, Ordering::SeqCst) {
                                            return;
                                        }
                                        d2.notify(Signal::Value(value.clone()));
                                    }
                                    Signal::Complete => d2.notify(Signal::Complete),
                                    Signal::Error(e) => d2.notify(Signal::Error(e.clone())),
                                }
                            }
                        });
                        d.own(guard2);
                    }
                    Signal::Error(e) => d.notify(Signal::Error(e.clone())),
                }
            }
        });
        derived.own(guard1);

        derived.lock()
    }
}

impl<T, W: Watchable<T>> ConcatExt<T> for W {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Mutable;
    use std::sync::Mutex;

    #[test]
    fn test_concat() {
        let first = Cell::new(1);
        let second = Cell::new(100);
        let combined = first.concat(&second);

        let emissions = Arc::new(Mutex::new(Vec::new()));
        let e = emissions.clone();
        let _guard = combined.subscribe(move |signal| {
            if let Signal::Value(v) = signal {
                e.lock().unwrap().push(**v);
            }
        });

        // Initial value from first
        assert_eq!(emissions.lock().unwrap().clone(), vec![1]);

        // Values from first
        first.set(2);
        first.set(3);
        assert_eq!(emissions.lock().unwrap().clone(), vec![1, 2, 3]);

        // Complete first - switches to second
        first.complete();

        // Second's current value is emitted on subscribe
        // (but we skip the first emission in the callback)

        // Values from second
        second.set(200);
        assert_eq!(emissions.lock().unwrap().clone(), vec![1, 2, 3, 200]);

        second.set(300);
        assert_eq!(emissions.lock().unwrap().clone(), vec![1, 2, 3, 200, 300]);
    }
}
