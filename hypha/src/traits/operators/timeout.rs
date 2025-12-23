use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::cell::{Cell, CellImmutable, CellMutable};
use crate::signal::Signal;

use super::Watchable;

pub trait TimeoutExt<T>: Watchable<T> {
    /// Error if no emission within the specified duration.
    ///
    /// Starts a timer after each emission. If no new emission arrives before
    /// the timer expires, emits an error signal.
    ///
    /// # Example
    ///
    /// ```
    /// use hypha::{Cell, Mutable, TimeoutExt, Signal, Watchable};
    /// use std::time::Duration;
    /// use std::sync::Arc;
    /// use std::sync::atomic::{AtomicBool, Ordering};
    ///
    /// let source = Cell::new(0);
    /// let timed = source.timeout(Duration::from_millis(100));
    ///
    /// let errored = Arc::new(AtomicBool::new(false));
    /// let e = errored.clone();
    /// let _guard = timed.subscribe(move |signal| {
    ///     if let Signal::Error(_) = signal {
    ///         e.store(true, Ordering::SeqCst);
    ///     }
    /// });
    ///
    /// // If we don't set within 100ms, it will error
    /// std::thread::sleep(Duration::from_millis(150));
    /// assert!(errored.load(Ordering::SeqCst));
    /// ```
    fn timeout(&self, duration: Duration) -> Cell<T, CellImmutable>
    where
        T: Clone + Send + Sync + 'static,
        Self: Clone + Send + Sync + 'static,
    {
        let derived = Cell::<T, CellMutable>::new(self.get());

        let weak = derived.downgrade();
        let generation = Arc::new(AtomicU64::new(0));
        let first = Arc::new(AtomicBool::new(true));
        let completed = Arc::new(AtomicBool::new(false));

        // Spawn initial timeout thread
        let gen_clone = generation.clone();
        let weak2 = derived.downgrade();
        let comp = completed.clone();
        thread::spawn(move || {
            let start_gen = gen_clone.load(Ordering::SeqCst);
            thread::sleep(duration);
            if !comp.load(Ordering::SeqCst)
                && gen_clone.load(Ordering::SeqCst) == start_gen
                && let Some(d) = weak2.upgrade()
            {
                d.notify(Signal::error(anyhow::anyhow!("timeout")));
            }
        });

        let guard = self.subscribe(move |signal| {
            if let Some(d) = weak.upgrade() {
                match signal {
                    Signal::Value(value) => {
                        if first.swap(false, Ordering::SeqCst) {
                            return;
                        }
                        // Increment generation to cancel pending timeout
                        let new_gen = generation.fetch_add(1, Ordering::SeqCst) + 1;
                        d.notify(Signal::Value(value.clone()));

                        // Spawn new timeout thread
                        let gen2 = generation.clone();
                        let weak3 = d.downgrade();
                        let comp = completed.clone();
                        thread::spawn(move || {
                            thread::sleep(duration);
                            if !comp.load(Ordering::SeqCst)
                                && gen2.load(Ordering::SeqCst) == new_gen
                                && let Some(d2) = weak3.upgrade()
                            {
                                d2.notify(Signal::error(anyhow::anyhow!("timeout")));
                            }
                        });
                    }
                    Signal::Complete => {
                        completed.store(true, Ordering::SeqCst);
                        d.notify(Signal::Complete);
                    }
                    Signal::Error(e) => {
                        completed.store(true, Ordering::SeqCst);
                        d.notify(Signal::Error(e.clone()));
                    }
                }
            }
        });
        derived.own(guard);

        derived.lock()
    }
}

impl<T, W: Watchable<T>> TimeoutExt<T> for W {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Mutable;
    use std::sync::atomic::AtomicU32;

    #[test]
    fn test_timeout_no_timeout_when_active() {
        let source = Cell::new(0);
        let timed = source.timeout(Duration::from_millis(50));

        let error_count = Arc::new(AtomicU32::new(0));
        let ec = error_count.clone();
        let _guard = timed.subscribe(move |signal| {
            if let Signal::Error(_) = signal {
                ec.fetch_add(1, Ordering::SeqCst);
            }
        });

        // Keep emitting within timeout
        for i in 1..=5 {
            thread::sleep(Duration::from_millis(20));
            source.set(i);
        }

        thread::sleep(Duration::from_millis(10));
        assert_eq!(error_count.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn test_timeout_triggers_on_inactivity() {
        let source = Cell::new(0);
        let timed = source.timeout(Duration::from_millis(30));

        let error_count = Arc::new(AtomicU32::new(0));
        let ec = error_count.clone();
        let _guard = timed.subscribe(move |signal| {
            if let Signal::Error(_) = signal {
                ec.fetch_add(1, Ordering::SeqCst);
            }
        });

        // Don't emit anything, wait for timeout
        thread::sleep(Duration::from_millis(50));
        assert_eq!(error_count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn test_timeout_no_error_after_complete() {
        let source = Cell::new(0);
        let timed = source.timeout(Duration::from_millis(30));

        let error_count = Arc::new(AtomicU32::new(0));
        let ec = error_count.clone();
        let _guard = timed.subscribe(move |signal| {
            if let Signal::Error(_) = signal {
                ec.fetch_add(1, Ordering::SeqCst);
            }
        });

        source.complete();
        thread::sleep(Duration::from_millis(50));
        // Should not error because stream completed
        assert_eq!(error_count.load(Ordering::SeqCst), 0);
    }
}
