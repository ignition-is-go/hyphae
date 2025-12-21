use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use crate::cell::{Cell, CellImmutable, CellMutable};
use super::{Gettable, Watchable};

pub trait DedupedExt<T>: Watchable<T> {
    fn deduped(&self) -> Cell<T, CellImmutable>
    where
        T: Clone + PartialEq + Send + Sync + 'static,
        Self: Clone + Send + Sync + 'static,
    {
        let derived = Cell::<T, CellMutable>::new(self.get());

        let weak = derived.downgrade();
        let first = Arc::new(AtomicBool::new(true));
        let guard = self.subscribe(move |value| {
            if first.swap(false, Ordering::SeqCst) {
                return;
            }
            if let Some(d) = weak.upgrade()
                && *value != d.get() {
                    d.notify(value.clone());
                }
        });
        derived.own(guard);

        // Propagate source completion
        let weak = derived.downgrade();
        let complete_guard = self.on_complete(move || {
            if let Some(d) = weak.upgrade() {
                d.complete();
            }
        });
        derived.own(complete_guard);

        derived.lock()
    }
}

impl<T, W: Watchable<T>> DedupedExt<T> for W {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Mutable;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    #[test]
    fn test_deduped_blocks_duplicates() {
        let source = Cell::new(1u64);
        let deduped = source.deduped();
        let count = Arc::new(AtomicU64::new(0));

        let c = count.clone();
        let _guard = deduped.subscribe(move |_| {
            c.fetch_add(1, Ordering::SeqCst);
        });

        assert_eq!(count.load(Ordering::SeqCst), 1); // initial

        source.set(1); // same value - blocked
        assert_eq!(count.load(Ordering::SeqCst), 1);

        source.set(2); // different - passes
        assert_eq!(count.load(Ordering::SeqCst), 2);

        source.set(2); // same - blocked
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }
}
