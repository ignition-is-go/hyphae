use std::{
    cell::UnsafeCell,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use super::{CellValue, Watchable};
use crate::{
    cell::{Cell, CellImmutable, CellMutable},
    signal::Signal,
};

/// A callback that can only be called once, implemented lock-free.
struct OnceCallback<F> {
    called: AtomicBool,
    callback: UnsafeCell<Option<F>>,
}

// Safety: The atomic bool ensures only one thread can access the callback
unsafe impl<F: Send> Send for OnceCallback<F> {}
unsafe impl<F: Send> Sync for OnceCallback<F> {}

impl<F: FnOnce()> OnceCallback<F> {
    fn new(f: F) -> Self {
        Self {
            called: AtomicBool::new(false),
            callback: UnsafeCell::new(Some(f)),
        }
    }

    fn call(&self) {
        if !self.called.swap(true, Ordering::SeqCst) {
            // Safety: called is now true, so no other thread can enter this block
            // The atomic swap ensures exclusive access to the UnsafeCell
            unsafe {
                if let Some(cb) = (*self.callback.get()).take() {
                    cb();
                }
            }
        }
    }
}

pub trait FinalizeExt<T>: Watchable<T> {
    /// Execute a callback when the stream completes or errors.
    ///
    /// The callback is called exactly once when either Complete or Error
    /// signal is received.
    ///
    /// # Example
    ///
    /// ```
    /// use hypha::{Cell, Mutable, FinalizeExt, Watchable};
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
    #[track_caller]
    fn finalize<F>(&self, callback: F) -> Cell<T, CellImmutable>
    where
        T: CellValue,
        F: FnOnce() + Send + Sync + 'static,
        Self: Clone + Send + Sync + 'static,
    {
        let derived = Cell::<T, CellMutable>::new(self.get());
        let derived = if let Some(name) = self.name() {
            derived.with_name(format!("{}::finalize", name))
        } else {
            derived
        };

        let weak = derived.downgrade();
        let first = Arc::new(AtomicBool::new(true));
        let callback = Arc::new(OnceCallback::new(callback));

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
                        callback.call();
                        d.notify(Signal::Complete);
                    }
                    Signal::Error(e) => {
                        callback.call();
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
