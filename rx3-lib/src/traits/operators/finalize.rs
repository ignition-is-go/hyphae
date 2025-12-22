use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::cell::{Cell, CellImmutable, CellMutable};
use crate::signal::Signal;

use super::Watchable;

pub trait FinalizeExt<T>: Watchable<T> {
    /// Execute a callback when the stream completes or errors.
    ///
    /// The callback is called exactly once when either Complete or Error
    /// signal is received.
    ///
    /// # Example
    ///
    /// ```
    /// use rx3::{Cell, Mutable, FinalizeExt, Watchable};
    /// use std::sync::Arc;
    /// use std::sync::atomic::{AtomicBool, Ordering};
    ///
    /// let source = Cell::new(0);
    /// let finalized_flag = Arc::new(AtomicBool::new(false));
    /// let flag = finalized_flag.clone();
    ///
    /// let finalized = source.finalize(move || {
    ///     flag.store(true, Ordering::SeqCst);
    /// });
    ///
    /// source.set(1);
    /// source.complete();
    /// assert!(finalized_flag.load(Ordering::SeqCst));
    /// ```
    fn finalize<F>(&self, callback: F) -> Cell<T, CellImmutable>
    where
        T: Clone + Send + Sync + 'static,
        F: FnOnce() + Send + Sync + 'static,
        Self: Clone + Send + Sync + 'static,
    {
        let derived = Cell::<T, CellMutable>::new(self.get());

        let weak = derived.downgrade();
        let first = Arc::new(AtomicBool::new(true));
        let callback = Arc::new(std::sync::Mutex::new(Some(callback)));

        let guard = self.subscribe(move |signal| {
            if let Some(d) = weak.upgrade() {
                match signal {
                    Signal::Value(value) => {
                        if first.swap(false, Ordering::SeqCst) {
                            return;
                        }
                        d.notify(Signal::Value(value.clone()));
                    }
                    Signal::Complete => {
                        if let Some(cb) = callback.lock().unwrap().take() {
                            cb();
                        }
                        d.notify(Signal::Complete);
                    }
                    Signal::Error(e) => {
                        if let Some(cb) = callback.lock().unwrap().take() {
                            cb();
                        }
                        d.notify(Signal::Error(e.clone()));
                    }
                }
            }
        });
        derived.own(guard);

        derived.lock()
    }
}

impl<T, W: Watchable<T>> FinalizeExt<T> for W {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Mutable;

    #[test]
    fn test_finalize_on_complete() {
        let source = Cell::new(0);
        let finalized = Arc::new(AtomicBool::new(false));

        let f = finalized.clone();
        let _finalized_cell = source.finalize(move || {
            f.store(true, Ordering::SeqCst);
        });

        assert!(!finalized.load(Ordering::SeqCst));

        source.complete();
        assert!(finalized.load(Ordering::SeqCst));
    }

    #[test]
    fn test_finalize_on_error() {
        let source = Cell::new(0);
        let finalized = Arc::new(AtomicBool::new(false));

        let f = finalized.clone();
        let _finalized_cell = source.finalize(move || {
            f.store(true, Ordering::SeqCst);
        });

        assert!(!finalized.load(Ordering::SeqCst));

        source.fail(anyhow::anyhow!("error"));
        assert!(finalized.load(Ordering::SeqCst));
    }

    #[test]
    fn test_finalize_called_once() {
        let source = Cell::new(0);
        let count = Arc::new(std::sync::atomic::AtomicU32::new(0));

        let c = count.clone();
        let _finalized_cell = source.finalize(move || {
            c.fetch_add(1, Ordering::SeqCst);
        });

        source.complete();
        source.complete(); // Second complete
        assert_eq!(count.load(Ordering::SeqCst), 1); // Only called once
    }
}
