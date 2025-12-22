use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::cell::{Cell, CellImmutable, CellMutable};
use crate::signal::Signal;

use super::Watchable;

pub trait SkipWhileExt<T>: Watchable<T> {
    /// Skip values while the predicate returns true.
    ///
    /// Once the predicate returns false, all subsequent values are emitted.
    ///
    /// # Example
    ///
    /// ```
    /// use hypha::{Cell, Mutable, SkipWhileExt, Watchable};
    ///
    /// let source = Cell::new(0);
    /// let skipped = source.skip_while(|v| *v < 3);
    ///
    /// source.set(1); // Skipped
    /// source.set(2); // Skipped
    /// source.set(3); // Passes (predicate false)
    /// source.set(1); // Passes (predicate was already false once)
    /// ```
    fn skip_while<F>(&self, predicate: F) -> Cell<T, CellImmutable>
    where
        T: Clone + Send + Sync + 'static,
        F: Fn(&T) -> bool + Send + Sync + 'static,
        Self: Clone + Send + Sync + 'static,
    {
        let derived = Cell::<T, CellMutable>::new(self.get());

        let weak = derived.downgrade();
        let skipping = Arc::new(AtomicBool::new(true));
        let first = Arc::new(AtomicBool::new(true));
        let predicate = Arc::new(predicate);

        let guard = self.subscribe(move |signal| {
            if let Some(d) = weak.upgrade() {
                match signal {
                    Signal::Value(value) => {
                        if first.swap(false, Ordering::SeqCst) {
                            return;
                        }
                        if skipping.load(Ordering::SeqCst) {
                            if !predicate(&**value) {
                                skipping.store(false, Ordering::SeqCst);
                                d.notify(Signal::Value(value.clone()));
                            }
                        } else {
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

impl<T, W: Watchable<T>> SkipWhileExt<T> for W {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Mutable;
    use std::sync::atomic::AtomicU32;

    #[test]
    fn test_skip_while() {
        let source = Cell::new(0);
        let skipped = source.skip_while(|v| *v < 3);

        let count = Arc::new(AtomicU32::new(0));
        let c = count.clone();
        let _guard = skipped.subscribe(move |signal| {
            if let Signal::Value(_) = signal {
                c.fetch_add(1, Ordering::SeqCst);
            }
        });

        assert_eq!(count.load(Ordering::SeqCst), 1); // Initial (0)

        source.set(1); // Skipped
        assert_eq!(count.load(Ordering::SeqCst), 1);

        source.set(2); // Skipped
        assert_eq!(count.load(Ordering::SeqCst), 1);

        source.set(3); // Passes
        assert_eq!(count.load(Ordering::SeqCst), 2);

        source.set(1); // Also passes (not skipping anymore)
        assert_eq!(count.load(Ordering::SeqCst), 3);
    }
}
