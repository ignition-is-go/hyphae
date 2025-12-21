use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use crate::cell::{Cell, CellImmutable};
use super::{DepNode, Watchable};

pub trait TakeWhileExt<T>: Watchable<T> {
    /// Take values while predicate returns true, then stop.
    fn take_while<F>(&self, predicate: F) -> Cell<T, CellImmutable>
    where
        T: Clone + Send + Sync + 'static,
        F: Fn(&T) -> bool + Send + Sync + 'static,
        Self: Clone + Send + Sync + 'static,
    {
        let initial = self.get();
        let parent: Arc<dyn DepNode> = Arc::new(self.clone());
        let derived = Cell::<T, CellImmutable>::derived(initial, vec![parent]);

        let stopped = Arc::new(AtomicBool::new(false));
        let weak = derived.downgrade();
        let first = Arc::new(AtomicBool::new(true));
        let guard = self.subscribe(move |value| {
            if first.swap(false, Ordering::SeqCst) {
                return;
            }
            if stopped.load(Ordering::SeqCst) {
                return;
            }
            if !predicate(value) {
                stopped.store(true, Ordering::SeqCst);
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

impl<T, W: Watchable<T>> TakeWhileExt<T> for W {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Gettable, Mutable};

    #[test]
    fn test_take_while() {
        let source = Cell::new(1u64);
        let taken = source.take_while(|x| *x < 5);

        assert_eq!(taken.get(), 1);

        source.set(3);
        assert_eq!(taken.get(), 3);

        source.set(5); // Predicate fails, stops
        assert_eq!(taken.get(), 3);

        source.set(2); // Even though valid, already stopped
        assert_eq!(taken.get(), 3);
    }
}
