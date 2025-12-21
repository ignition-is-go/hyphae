use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use crate::cell::{Cell, CellImmutable};
use super::{DepNode, Watchable};

pub trait FirstExt<T>: Watchable<T> {
    /// Take only the first value, then stop.
    fn first(&self) -> Cell<T, CellImmutable>
    where
        T: Clone + Send + Sync + 'static,
        Self: Clone + Send + Sync + 'static,
    {
        let parent: Arc<dyn DepNode> = Arc::new(self.clone());
        let derived = Cell::<T, CellImmutable>::derived(self.get(), vec![parent]);

        // Already got first value, no need to subscribe for more
        // But we do subscribe once to establish the dependency
        let taken = Arc::new(AtomicBool::new(true));
        let weak = derived.downgrade();
        let guard = self.subscribe(move |_| {
            if taken.swap(true, Ordering::SeqCst) {
                return; // Already took first
            }
            // This won't run - we already have the value
            if let Some(_) = weak.upgrade() {}
        });
        derived.own(guard);

        derived
    }
}

impl<T, W: Watchable<T>> FirstExt<T> for W {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Gettable, Mutable};

    #[test]
    fn test_first() {
        let source = Cell::new(42);
        let first = source.first();

        assert_eq!(first.get(), 42);

        source.set(100);
        assert_eq!(first.get(), 42); // Still 42, only takes first
    }
}
