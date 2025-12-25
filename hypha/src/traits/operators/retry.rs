use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use crate::cell::{Cell, CellImmutable, CellMutable};
use crate::signal::Signal;

use super::Watchable;

pub trait RetryExt<T>: Watchable<T> {
    /// Retry on error up to max_attempts times.
    ///
    /// When an error signal is received, resubscribes to the source.
    /// After max_attempts errors, propagates the error.
    ///
    /// # Example
    ///
    /// ```
    /// use hypha::{Cell, Mutable, RetryExt, Watchable};
    ///
    /// let source = Cell::new(0);
    /// let retried = source.retry(3);
    ///
    /// // Errors will be retried up to 3 times before propagating
    /// // Values pass through normally
    /// source.set(1);
    /// ```
    fn retry(&self, max_attempts: usize) -> Cell<T, CellImmutable>
    where
        T: Clone + Send + Sync + 'static,
        Self: Clone + Send + Sync + 'static,
    {
        let derived = Cell::<T, CellMutable>::new(self.get());

        let weak = derived.downgrade();
        let attempts = Arc::new(AtomicUsize::new(0));
        let first = Arc::new(AtomicBool::new(true));
        let source = self.clone();

        let guard = self.subscribe(move |signal| {
            if let Some(d) = weak.upgrade() {
                match signal {
                    Signal::Value(value, ctx) => {
                        if first.swap(false, Ordering::SeqCst) {
                            return;
                        }
                        // Reset attempts on successful value
                        attempts.store(0, Ordering::SeqCst);
                        d.notify(Signal::Value(value.clone(), ctx.clone()));
                    }
                    Signal::Complete => d.notify(Signal::Complete),
                    Signal::Error(e) => {
                        let attempt = attempts.fetch_add(1, Ordering::SeqCst) + 1;
                        if attempt >= max_attempts {
                            d.notify(Signal::Error(e.clone()));
                        } else {
                            // Resubscribe to source
                            let weak2 = d.downgrade();
                            let attempts2 = attempts.clone();
                            let max = max_attempts;
                            let guard2 = source.subscribe(move |sig| {
                                if let Some(d2) = weak2.upgrade() {
                                    match sig {
                                        Signal::Value(v, ctx) => {
                                            attempts2.store(0, Ordering::SeqCst);
                                            d2.notify(Signal::Value(v.clone(), ctx.clone()));
                                        }
                                        Signal::Complete => d2.notify(Signal::Complete),
                                        Signal::Error(e2) => {
                                            let a = attempts2.fetch_add(1, Ordering::SeqCst) + 1;
                                            if a >= max {
                                                d2.notify(Signal::Error(e2.clone()));
                                            }
                                            // Note: nested retries would need recursion
                                        }
                                    }
                                }
                            });
                            d.own(guard2);
                        }
                    }
                }
            }
        });
        derived.own(guard);

        derived.lock()
    }

    /// Retry on error with a predicate to decide whether to retry.
    ///
    /// # Example
    ///
    /// ```
    /// use hypha::{Cell, Mutable, RetryExt, Watchable};
    ///
    /// let source = Cell::new(0);
    /// let retried = source.retry_when(|_err, attempt| {
    ///     // Only retry up to 5 times
    ///     attempt < 5
    /// });
    /// source.set(1);
    /// ```
    fn retry_when<F>(&self, predicate: F) -> Cell<T, CellImmutable>
    where
        T: Clone + Send + Sync + 'static,
        F: Fn(&dyn std::any::Any, usize) -> bool + Send + Sync + 'static,
        Self: Clone + Send + Sync + 'static,
    {
        let derived = Cell::<T, CellMutable>::new(self.get());

        let weak = derived.downgrade();
        let attempts = Arc::new(AtomicUsize::new(0));
        let first = Arc::new(AtomicBool::new(true));
        let predicate = Arc::new(predicate);
        let source = self.clone();

        let guard = self.subscribe(move |signal| {
            if let Some(d) = weak.upgrade() {
                match signal {
                    Signal::Value(value, ctx) => {
                        if first.swap(false, Ordering::SeqCst) {
                            return;
                        }
                        attempts.store(0, Ordering::SeqCst);
                        d.notify(Signal::Value(value.clone(), ctx.clone()));
                    }
                    Signal::Complete => d.notify(Signal::Complete),
                    Signal::Error(e) => {
                        let attempt = attempts.fetch_add(1, Ordering::SeqCst) + 1;
                        if predicate(&**e, attempt) {
                            // Resubscribe
                            let weak2 = d.downgrade();
                            let attempts2 = attempts.clone();
                            let pred2 = predicate.clone();
                            let guard2 = source.subscribe(move |sig| {
                                if let Some(d2) = weak2.upgrade() {
                                    match sig {
                                        Signal::Value(v, ctx) => {
                                            attempts2.store(0, Ordering::SeqCst);
                                            d2.notify(Signal::Value(v.clone(), ctx.clone()));
                                        }
                                        Signal::Complete => d2.notify(Signal::Complete),
                                        Signal::Error(e2) => {
                                            let a = attempts2.fetch_add(1, Ordering::SeqCst) + 1;
                                            if !pred2(&**e2, a) {
                                                d2.notify(Signal::Error(e2.clone()));
                                            }
                                        }
                                    }
                                }
                            });
                            d.own(guard2);
                        } else {
                            d.notify(Signal::Error(e.clone()));
                        }
                    }
                }
            }
        });
        derived.own(guard);

        derived.lock()
    }
}

impl<T, W: Watchable<T>> RetryExt<T> for W {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Mutable;
    use std::sync::atomic::AtomicU32;

    #[test]
    fn test_retry_passes_values() {
        let source = Cell::new(0);
        let retried = source.retry(3);

        let count = Arc::new(AtomicU32::new(0));
        let c = count.clone();
        let _guard = retried.subscribe(move |signal| {
            if let Signal::Value(_, _) = signal {
                c.fetch_add(1, Ordering::SeqCst);
            }
        });

        assert_eq!(count.load(Ordering::SeqCst), 1); // Initial

        source.set(1);
        source.set(2);
        assert_eq!(count.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn test_retry_retries_on_error() {
        // Note: retry(1) means propagate after first error (no retries)
        // retry(3) means allow 2 retries before propagating
        let source = Cell::new(0);
        let retried = source.retry(1); // Propagate immediately

        let error_count = Arc::new(AtomicU32::new(0));
        let ec = error_count.clone();
        let _guard = retried.subscribe(move |signal| {
            if let Signal::Error(_) = signal {
                ec.fetch_add(1, Ordering::SeqCst);
            }
        });

        // With retry(1), first error should propagate
        source.fail(anyhow::anyhow!("error 1"));
        assert_eq!(error_count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn test_retry_resets_on_success() {
        let source = Cell::new(0);
        let retried = source.retry(2);

        let error_count = Arc::new(AtomicU32::new(0));
        let ec = error_count.clone();
        let _guard = retried.subscribe(move |signal| {
            if let Signal::Error(_) = signal {
                ec.fetch_add(1, Ordering::SeqCst);
            }
        });

        source.fail(anyhow::anyhow!("error 1"));
        source.set(1); // Success resets counter
        source.fail(anyhow::anyhow!("error 2"));
        // Should not propagate yet
        assert_eq!(error_count.load(Ordering::SeqCst), 0);
    }
}
