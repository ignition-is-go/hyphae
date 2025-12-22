use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use crate::cell::{Cell, CellImmutable, CellMutable};
use crate::signal::Signal;
use super::Watchable;

/// Extension trait for fallible transformations.
pub trait TryMapExt<T>: Watchable<T> {
    /// Transform values with a fallible function.
    ///
    /// Returns a `Cell<Result<U, E>>` that contains `Ok(value)` when the
    /// transform succeeds, or `Err(error)` when it fails.
    ///
    /// # Example
    /// ```
    /// use hypha::{Cell, Mutable, TryMapExt, Gettable};
    ///
    /// let source = Cell::new(10i32);
    /// let parsed = source.try_map(|v| {
    ///     if *v > 0 {
    ///         Ok(v.to_string())
    ///     } else {
    ///         Err("must be positive")
    ///     }
    /// });
    ///
    /// assert_eq!(parsed.get(), Ok("10".to_string()));
    /// ```
    fn try_map<U, E, F>(&self, f: F) -> Cell<Result<U, E>, CellImmutable>
    where
        T: 'static,
        U: Clone + Send + Sync + 'static,
        E: Clone + Send + Sync + 'static,
        F: Fn(&T) -> Result<U, E> + Send + Sync + 'static,
        Self: Clone + Send + Sync + 'static,
    {
        let initial = f(&self.get());
        let derived = Cell::<Result<U, E>, CellMutable>::new(initial);

        let weak = derived.downgrade();
        let first = Arc::new(AtomicBool::new(true));
        let guard = self.subscribe(move |signal| {
            if let Some(d) = weak.upgrade() {
                match signal {
                    Signal::Value(value) => {
                        if first.swap(false, Ordering::SeqCst) {
                            return;
                        }
                        d.notify(Signal::value(f(value.as_ref())));
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

impl<T, W: Watchable<T>> TryMapExt<T> for W {}
