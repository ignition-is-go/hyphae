use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use crate::cell::{Cell, CellImmutable, CellMutable};
use crate::signal::Signal;
use super::Watchable;

pub trait TakeUntilExt<T>: Watchable<T> {
    /// Take values until the notifier emits, then stop.
    fn take_until<U, M>(&self, notifier: &Cell<U, M>) -> Cell<T, CellImmutable>
    where
        T: Clone + Send + Sync + 'static,
        U: Clone + Send + Sync + 'static,
        M: Send + Sync + 'static,
        Self: Clone + Send + Sync + 'static,
    {
        let initial = self.get();
        let derived = Cell::<T, CellMutable>::new(initial);

        let stopped = Arc::new(AtomicBool::new(false));

        // Subscribe to notifier - when it emits, stop
        let stopped_clone = stopped.clone();
        let notifier_first = Arc::new(AtomicBool::new(true));
        let weak_for_notifier = derived.downgrade();
        let notifier_guard = notifier.subscribe(move |signal| {
            // Only react to values, ignore notifier's complete/error
            if let Signal::Value(_, _) = signal {
                if notifier_first.swap(false, Ordering::SeqCst) {
                    return;
                }
                stopped_clone.store(true, Ordering::SeqCst);
                if let Some(d) = weak_for_notifier.upgrade() {
                    d.notify(Signal::Complete);
                }
            }
        });
        derived.own(notifier_guard);

        // Subscribe to source
        let weak = derived.downgrade();
        let first = Arc::new(AtomicBool::new(true));
        let guard = self.subscribe(move |signal| {
            if let Some(d) = weak.upgrade() {
                match signal {
                    Signal::Value(_, _) => {
                        if first.swap(false, Ordering::SeqCst) {
                            return;
                        }
                        if stopped.load(Ordering::SeqCst) {
                            return;
                        }
                        d.notify(signal.clone());
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

impl<T, W: Watchable<T>> TakeUntilExt<T> for W {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Gettable, Mutable};

    #[test]
    fn test_take_until() {
        let source = Cell::new(1u64);
        let stopper = Cell::new(false);
        let taken = source.take_until(&stopper);

        assert_eq!(taken.get(), 1);

        source.set(2);
        assert_eq!(taken.get(), 2);

        stopper.set(true); // Signal stop

        source.set(3);
        assert_eq!(taken.get(), 2); // Stopped, no more updates
    }

    #[test]
    fn test_take_until_completes_on_notifier() {
        use std::sync::atomic::AtomicBool;

        let source = Cell::new(1u64);
        let stopper = Cell::new(false);
        let taken = source.take_until(&stopper);
        let completed = Arc::new(AtomicBool::new(false));

        let c = completed.clone();
        let _guard = taken.subscribe(move |signal| {
            if let Signal::Complete = signal {
                c.store(true, Ordering::SeqCst);
            }
        });

        assert!(!taken.is_complete());

        stopper.set(true); // Signal stop

        assert!(taken.is_complete());
        assert!(completed.load(Ordering::SeqCst));
    }
}
