use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use crate::cell::{Cell, CellImmutable, CellMutable};
use super::Watchable;

pub trait FilterExt<T>: Watchable<T> {
    fn filter(&self, predicate: impl Fn(&T) -> bool + Send + Sync + 'static) -> Cell<T, CellImmutable>
    where
        T: Clone + Send + Sync + 'static,
        Self: Clone + Send + Sync + 'static,
    {
        let cell = Cell::<T, CellMutable>::new(self.get());

        let weak = cell.downgrade();
        let predicate = Arc::new(predicate);
        let first = Arc::new(AtomicBool::new(true));
        let guard = self.subscribe(move |value| {
            if first.swap(false, Ordering::SeqCst) {
                return;
            }
            if let Some(c) = weak.upgrade() {
                if predicate(value) {
                    c.notify(value.clone());
                }
            }
        });
        cell.own(guard);

        // Propagate source completion
        let weak = cell.downgrade();
        let complete_guard = self.on_complete(move || {
            if let Some(c) = weak.upgrade() {
                c.complete();
            }
        });
        cell.own(complete_guard);

        cell.lock()
    }
}

impl<T, W: Watchable<T>> FilterExt<T> for W {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Mutable;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    #[test]
    fn test_filter_passes_matching() {
        let source = Cell::new(10);
        let evens = source.filter(|x| x % 2 == 0);
        let received = Arc::new(AtomicU64::new(0));

        let r = received.clone();
        let _guard = evens.subscribe(move |v| {
            r.store(*v, Ordering::SeqCst);
        });

        assert_eq!(received.load(Ordering::SeqCst), 10);

        source.set(4);
        assert_eq!(received.load(Ordering::SeqCst), 4);
    }

    #[test]
    fn test_filter_blocks_non_matching() {
        let source = Cell::new(10u64);
        let evens = source.filter(|x| x % 2 == 0);
        let received = Arc::new(AtomicU64::new(0));

        let r = received.clone();
        let _guard = evens.subscribe(move |v| {
            r.store(*v, Ordering::SeqCst);
        });

        source.set(3); // odd - should not pass
        assert_eq!(received.load(Ordering::SeqCst), 10); // still 10

        source.set(6); // even - should pass
        assert_eq!(received.load(Ordering::SeqCst), 6);
    }
}
