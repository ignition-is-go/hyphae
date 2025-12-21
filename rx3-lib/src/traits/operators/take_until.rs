use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use crate::cell::{Cell, CellImmutable};
use super::{DepNode, Watchable};

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
        let parent: Arc<dyn DepNode> = Arc::new(self.clone());
        let derived = Cell::<T, CellImmutable>::derived(initial, vec![parent]);

        let stopped = Arc::new(AtomicBool::new(false));

        // Subscribe to notifier - when it emits, stop
        let stopped_clone = stopped.clone();
        let notifier_first = Arc::new(AtomicBool::new(true));
        let notifier_guard = notifier.subscribe(move |_| {
            if notifier_first.swap(false, Ordering::SeqCst) {
                return;
            }
            stopped_clone.store(true, Ordering::SeqCst);
        });
        derived.own(notifier_guard);

        // Subscribe to source
        let weak = derived.downgrade();
        let first = Arc::new(AtomicBool::new(true));
        let guard = self.subscribe(move |value| {
            if first.swap(false, Ordering::SeqCst) {
                return;
            }
            if stopped.load(Ordering::SeqCst) {
                return;
            }
            if let Some(d) = weak.upgrade() {
                d.notify(value.clone());
            }
        });
        derived.own(guard);

        derived
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
}
