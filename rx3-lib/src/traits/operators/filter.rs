use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use crate::cell::{Cell, CellImmutable};
use super::{DepNode, Watchable};

pub trait FilterExt<T>: Watchable<T> {
    fn filter(&self, predicate: impl Fn(&T) -> bool + Send + Sync + 'static) -> Cell<T, CellImmutable>
    where
        T: Clone + Send + Sync + 'static,
        Self: Clone + Send + Sync + 'static,
    {
        let parent: Arc<dyn DepNode> = Arc::new(self.clone());
        let cell = Cell::<T, CellImmutable>::derived(self.get(), vec![parent]);

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

        cell
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
