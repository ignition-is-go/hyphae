use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use crate::cell::{Cell, CellImmutable, CellMutable};
use crate::signal::Signal;
use super::Watchable;

pub trait TakeExt<T>: Watchable<T> {
    fn take(&self, count: usize) -> Cell<T, CellImmutable>
    where
        T: Clone + Send + Sync + 'static,
        Self: Clone + Send + Sync + 'static,
    {
        let cell = Cell::<T, CellMutable>::new(self.get());

        let remaining = Arc::new(AtomicUsize::new(count));
        let weak = cell.downgrade();
        let guard = self.subscribe(move |signal| {
            if let Some(c) = weak.upgrade() {
                match signal {
                    Signal::Value(value) => {
                        let prev = remaining.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                            if n > 0 { Some(n - 1) } else { None }
                        });
                        match prev {
                            Ok(1) => {
                                // This was the last one
                                c.notify(Signal::Value(value.clone()));
                                c.notify(Signal::Complete);
                            }
                            Ok(_) => {
                                c.notify(Signal::Value(value.clone()));
                            }
                            Err(_) => {
                                // Already exhausted
                            }
                        }
                    }
                    Signal::Complete => c.notify(Signal::Complete),
                    Signal::Error(e) => c.notify(Signal::Error(e.clone())),
                }
            }
        });
        cell.own(guard);

        cell.lock()
    }
}

impl<T, W: Watchable<T>> TakeExt<T> for W {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Mutable;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering as AtomicOrdering};
    use std::sync::Arc;

    #[test]
    fn test_take_limits_emissions() {
        let source = Cell::new(0u64);
        let taken = source.take(3);
        let count = Arc::new(AtomicU64::new(0));

        let c = count.clone();
        let _guard = taken.subscribe(move |signal| {
            if let Signal::Value(_) = signal {
                c.fetch_add(1, AtomicOrdering::SeqCst);
            }
        });

        // Initial watch call counts as 1
        assert_eq!(count.load(AtomicOrdering::SeqCst), 1);

        source.set(1); // 2
        source.set(2); // 3
        source.set(3); // ignored
        source.set(4); // ignored

        assert_eq!(count.load(AtomicOrdering::SeqCst), 3);
    }

    #[test]
    fn test_take_completes() {
        let source = Cell::new(0u64);
        let taken = source.take(2);
        let completed = Arc::new(AtomicBool::new(false));

        let c = completed.clone();
        let _guard = taken.subscribe(move |signal| {
            if let Signal::Complete = signal {
                c.store(true, AtomicOrdering::SeqCst);
            }
        });

        assert!(!taken.is_complete());
        assert!(!completed.load(AtomicOrdering::SeqCst));

        source.set(1); // 2nd emission, completes

        assert!(taken.is_complete());
        assert!(completed.load(AtomicOrdering::SeqCst));
    }
}
