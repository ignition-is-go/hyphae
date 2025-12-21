use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use crate::cell::{Cell, CellImmutable};
use super::{DepNode, Watchable};

pub trait MergeExt<T>: Watchable<T> {
    /// Merge with another cell - emit from whichever updates.
    fn merge<M>(&self, other: &Cell<T, M>) -> Cell<T, CellImmutable>
    where
        T: Clone + Send + Sync + 'static,
        M: Send + Sync + 'static,
        Self: Clone + Send + Sync + 'static,
    {
        let initial = self.get();
        let deps: Vec<Arc<dyn DepNode>> = vec![
            Arc::new(self.clone()),
            Arc::new(other.clone()),
        ];
        let derived = Cell::<T, CellImmutable>::derived(initial, deps);

        // Subscribe to self
        let weak1 = derived.downgrade();
        let first1 = Arc::new(AtomicBool::new(true));
        let guard1 = self.subscribe(move |value| {
            if first1.swap(false, Ordering::SeqCst) {
                return;
            }
            if let Some(d) = weak1.upgrade() {
                d.notify(value.clone());
            }
        });
        derived.own(guard1);

        // Subscribe to other
        let weak2 = derived.downgrade();
        let first2 = Arc::new(AtomicBool::new(true));
        let guard2 = other.subscribe(move |value| {
            if first2.swap(false, Ordering::SeqCst) {
                return;
            }
            if let Some(d) = weak2.upgrade() {
                d.notify(value.clone());
            }
        });
        derived.own(guard2);

        derived
    }
}

impl<T, W: Watchable<T>> MergeExt<T> for W {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Gettable, Mutable};

    #[test]
    fn test_merge() {
        let a = Cell::new(1);
        let b = Cell::new(10);
        let merged = a.merge(&b);

        assert_eq!(merged.get(), 1); // Initial from a

        a.set(2);
        assert_eq!(merged.get(), 2);

        b.set(20);
        assert_eq!(merged.get(), 20);

        a.set(3);
        assert_eq!(merged.get(), 3);
    }
}
