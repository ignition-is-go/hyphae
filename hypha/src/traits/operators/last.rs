use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use arc_swap::ArcSwap;

use super::Watchable;
use crate::{
    cell::{Cell, CellImmutable, CellMutable},
    signal::Signal,
};

pub trait LastExt<T>: Watchable<T> {
    /// Emit only the last value before completion.
    ///
    /// Stores each value as it arrives, but only emits when the source completes.
    /// If the source completes without emitting any values after the initial,
    /// the initial value is used.
    ///
    /// # Example
    ///
    /// ```
    /// use hypha::{Cell, Mutable, Gettable, LastExt};
    ///
    /// let source = Cell::new(0);
    /// let last = source.last();
    ///
    /// source.set(1);
    /// source.set(2);
    /// source.set(3);
    /// source.complete(); // Emits 3
    ///
    /// assert_eq!(last.get(), 3);
    /// ```
    fn last(&self) -> Cell<T, CellImmutable>
    where
        T: Clone + Send + Sync + 'static,
        Self: Clone + Send + Sync + 'static,
    {
        let derived = Cell::<T, CellMutable>::new(self.get());

        let weak = derived.downgrade();
        let last_value: Arc<ArcSwap<T>> = Arc::new(ArcSwap::from_pointee(self.get()));
        let first = Arc::new(AtomicBool::new(true));

        let guard = self.subscribe(move |signal| {
            if let Some(d) = weak.upgrade() {
                match signal {
                    Signal::Value(value) => {
                        if first.swap(false, Ordering::SeqCst) {
                            return;
                        }
                        // Just store, don't emit
                        last_value.store(value.clone());
                    }
                    Signal::Complete => {
                        // Emit the last stored value
                        let val = last_value.load();
                        d.notify(Signal::value((**val).clone()));
                        d.notify(Signal::Complete);
                    }
                    Signal::Error(e) => d.notify(Signal::Error(e.clone())),
                }
            }
        });
        derived.own(guard);

        derived.lock()
    }

    /// Emit only the last value, or a default if no values were emitted.
    ///
    /// # Example
    ///
    /// ```
    /// use hypha::{Cell, Mutable, Gettable, LastExt};
    ///
    /// let source = Cell::new(0);
    /// let last = source.last_or(999);
    ///
    /// source.complete(); // No values set, emits 999
    ///
    /// assert_eq!(last.get(), 999);
    /// ```
    fn last_or(&self, default: T) -> Cell<T, CellImmutable>
    where
        T: Clone + Send + Sync + 'static,
        Self: Clone + Send + Sync + 'static,
    {
        let derived = Cell::<T, CellMutable>::new(self.get());

        let weak = derived.downgrade();
        let last_value: Arc<ArcSwap<Option<T>>> = Arc::new(ArcSwap::from_pointee(None));
        let first = Arc::new(AtomicBool::new(true));

        let guard = self.subscribe(move |signal| {
            if let Some(d) = weak.upgrade() {
                match signal {
                    Signal::Value(value) => {
                        if first.swap(false, Ordering::SeqCst) {
                            return;
                        }
                        last_value.store(Arc::new(Some((**value).clone())));
                    }
                    Signal::Complete => {
                        let val = last_value.load();
                        let emit = match &**val {
                            Some(v) => v.clone(),
                            None => default.clone(),
                        };
                        d.notify(Signal::value(emit));
                        d.notify(Signal::Complete);
                    }
                    Signal::Error(e) => d.notify(Signal::Error(e.clone())),
                }
            }
        });
        derived.own(guard);

        derived.lock()
    }
}

impl<T, W: Watchable<T>> LastExt<T> for W {}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;
    use crate::{Gettable, Mutable};

    #[test]
    fn test_last() {
        let source = Cell::new(0);
        let last = source.last();

        let value_emissions = Arc::new(AtomicU32::new(0));
        let ve = value_emissions.clone();
        let _guard = last.subscribe(move |signal| {
            if let Signal::Value(_) = signal {
                ve.fetch_add(1, Ordering::SeqCst);
            }
        });

        assert_eq!(value_emissions.load(Ordering::SeqCst), 1); // Initial

        source.set(1);
        source.set(2);
        source.set(3);
        // No emissions yet - just storing
        assert_eq!(value_emissions.load(Ordering::SeqCst), 1);

        source.complete();
        // Now emits the last value
        assert_eq!(value_emissions.load(Ordering::SeqCst), 2);
        assert_eq!(last.get(), 3);
    }

    #[test]
    fn test_last_or_with_values() {
        let source = Cell::new(0);
        let last = source.last_or(999);

        source.set(1);
        source.set(2);
        source.complete();

        assert_eq!(last.get(), 2);
    }

    #[test]
    fn test_last_or_without_values() {
        let source = Cell::new(0);
        let last = source.last_or(999);

        // No set() calls, just complete
        source.complete();

        assert_eq!(last.get(), 999);
    }
}
