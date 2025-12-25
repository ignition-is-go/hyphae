use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crossbeam::queue::ArrayQueue;

use crate::cell::{Cell, CellImmutable, CellMutable};
use crate::signal::Signal;

use super::Watchable;

pub trait BackpressureExt<T>: Watchable<T> {
    /// Buffer values with drop-oldest strategy when capacity is reached.
    ///
    /// When the buffer is full, the oldest value is discarded to make room.
    /// Uses lock-free `ArrayQueue` internally.
    ///
    /// # Example
    ///
    /// ```
    /// use hypha::{Cell, Mutable, Gettable, BackpressureExt};
    ///
    /// let source = Cell::new(0);
    /// let buffered = source.drop_oldest(3);
    ///
    /// // Fast producer, slow consumer scenario
    /// source.set(1);
    /// source.set(2);
    /// source.set(3);
    /// source.set(4); // Drops 1, keeps 2,3,4
    /// ```
    fn drop_oldest(&self, capacity: usize) -> Cell<T, CellImmutable>
    where
        T: Clone + Send + Sync + 'static,
        Self: Clone + Send + Sync + 'static,
    {
        assert!(capacity > 0, "capacity must be positive");

        let derived = Cell::<T, CellMutable>::new(self.get());

        let weak = derived.downgrade();
        let buffer: Arc<ArrayQueue<T>> = Arc::new(ArrayQueue::new(capacity));
        let first = Arc::new(AtomicBool::new(true));

        let guard = self.subscribe(move |signal| {
            if let Some(d) = weak.upgrade() {
                match signal {
                    Signal::Value(value, _) => {
                        if first.swap(false, Ordering::SeqCst) {
                            return;
                        }
                        // Try to push, if full drop oldest and retry
                        let val = (**value).clone();
                        if buffer.push(val.clone()).is_err() {
                            // Buffer full - drop oldest
                            let _ = buffer.pop();
                            let _ = buffer.push(val.clone());
                        }
                        d.notify(Signal::value(val));
                    }
                    Signal::Complete => d.notify(Signal::Complete),
                    Signal::Error(e) => d.notify(Signal::Error(e.clone())),
                }
            }
        });
        derived.own(guard);

        derived.lock()
    }

    /// Buffer values with drop-newest strategy when capacity is reached.
    ///
    /// When the buffer is full, new values are discarded (not buffered).
    /// Uses lock-free `ArrayQueue` internally.
    ///
    /// # Example
    ///
    /// ```
    /// use hypha::{Cell, Mutable, Gettable, BackpressureExt};
    ///
    /// let source = Cell::new(0);
    /// let buffered = source.drop_newest(3);
    ///
    /// // Fast producer, slow consumer scenario
    /// source.set(1);
    /// source.set(2);
    /// source.set(3);
    /// source.set(4); // Dropped, buffer has 1,2,3
    /// ```
    fn drop_newest(&self, capacity: usize) -> Cell<T, CellImmutable>
    where
        T: Clone + Send + Sync + 'static,
        Self: Clone + Send + Sync + 'static,
    {
        assert!(capacity > 0, "capacity must be positive");

        let derived = Cell::<T, CellMutable>::new(self.get());

        let weak = derived.downgrade();
        let buffer: Arc<ArrayQueue<T>> = Arc::new(ArrayQueue::new(capacity));
        let first = Arc::new(AtomicBool::new(true));

        let guard = self.subscribe(move |signal| {
            if let Some(d) = weak.upgrade() {
                match signal {
                    Signal::Value(value, _) => {
                        if first.swap(false, Ordering::SeqCst) {
                            return;
                        }
                        let val = (**value).clone();
                        // Try to push, if full just drop (don't notify)
                        if buffer.push(val.clone()).is_ok() {
                            d.notify(Signal::value(val));
                        }
                        // If push failed, value is dropped (newest dropped)
                    }
                    Signal::Complete => d.notify(Signal::Complete),
                    Signal::Error(e) => d.notify(Signal::Error(e.clone())),
                }
            }
        });
        derived.own(guard);

        derived.lock()
    }

    /// Keep only the latest value - consumer reads at its own pace.
    ///
    /// This is useful when you only care about the most recent value
    /// and intermediate values can be skipped. The consumer can call
    /// `get()` whenever ready to read the latest.
    ///
    /// Note: This is essentially what a regular Cell already does via
    /// `ArcSwap`. This operator is provided for API consistency.
    ///
    /// # Example
    ///
    /// ```
    /// use hypha::{Cell, Mutable, Gettable, BackpressureExt};
    ///
    /// let source = Cell::new(0);
    /// let latest = source.sample_latest();
    ///
    /// source.set(1);
    /// source.set(2);
    /// source.set(3);
    /// // Consumer reads when ready - gets 3, skipped 1 and 2
    /// assert_eq!(latest.get(), 3);
    /// ```
    fn sample_latest(&self) -> Cell<T, CellImmutable>
    where
        T: Clone + Send + Sync + 'static,
        Self: Clone + Send + Sync + 'static,
    {
        // Since Cell already uses ArcSwap, this is essentially a passthrough
        // that creates a derived cell holding the latest value
        let derived = Cell::<T, CellMutable>::new(self.get());

        let weak = derived.downgrade();
        let first = Arc::new(AtomicBool::new(true));

        let guard = self.subscribe(move |signal| {
            if let Some(d) = weak.upgrade() {
                match signal {
                    Signal::Value(value, ctx) => {
                        if first.swap(false, Ordering::SeqCst) {
                            return;
                        }
                        // Just update to latest, no buffering, preserve transaction context
                        d.notify(Signal::Value(value.clone(), ctx.clone()));
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

impl<T, W: Watchable<T>> BackpressureExt<T> for W {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Gettable, Mutable};
    use std::sync::atomic::{AtomicU32, Ordering};

    #[test]
    fn test_drop_oldest() {
        let source = Cell::new(0);
        let buffered = source.drop_oldest(3);

        let count = Arc::new(AtomicU32::new(0));
        let c = count.clone();
        let _guard = buffered.subscribe(move |_| {
            c.fetch_add(1, Ordering::SeqCst);
        });

        assert_eq!(count.load(Ordering::SeqCst), 1); // Initial

        source.set(1);
        source.set(2);
        source.set(3);
        assert_eq!(count.load(Ordering::SeqCst), 4);

        // Buffer full, but drop_oldest still emits (drops from buffer)
        source.set(4);
        assert_eq!(count.load(Ordering::SeqCst), 5);
    }

    #[test]
    fn test_drop_newest() {
        let source = Cell::new(0);
        let buffered = source.drop_newest(3);

        let count = Arc::new(AtomicU32::new(0));
        let c = count.clone();
        let _guard = buffered.subscribe(move |_| {
            c.fetch_add(1, Ordering::SeqCst);
        });

        assert_eq!(count.load(Ordering::SeqCst), 1); // Initial

        source.set(1);
        source.set(2);
        source.set(3);
        assert_eq!(count.load(Ordering::SeqCst), 4);

        // Buffer full - new values dropped, no emission
        source.set(4);
        assert_eq!(count.load(Ordering::SeqCst), 4); // Still 4

        source.set(5);
        assert_eq!(count.load(Ordering::SeqCst), 4); // Still 4
    }

    #[test]
    fn test_sample_latest() {
        let source = Cell::new(0);
        let latest = source.sample_latest();

        source.set(1);
        source.set(2);
        source.set(3);

        // Latest value is 3
        assert_eq!(latest.get(), 3);
    }

    #[test]
    fn test_drop_oldest_forwards_complete() {
        let source = Cell::new(0);
        let buffered = source.drop_oldest(3);

        let completed = Arc::new(AtomicBool::new(false));
        let c = completed.clone();
        let _guard = buffered.subscribe(move |signal| {
            if let Signal::Complete = signal {
                c.store(true, Ordering::SeqCst);
            }
        });

        source.complete();
        assert!(completed.load(Ordering::SeqCst));
    }

    #[test]
    fn test_drop_newest_forwards_complete() {
        let source = Cell::new(0);
        let buffered = source.drop_newest(3);

        let completed = Arc::new(AtomicBool::new(false));
        let c = completed.clone();
        let _guard = buffered.subscribe(move |signal| {
            if let Signal::Complete = signal {
                c.store(true, Ordering::SeqCst);
            }
        });

        source.complete();
        assert!(completed.load(Ordering::SeqCst));
    }
}
