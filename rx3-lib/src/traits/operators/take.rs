use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use crate::cell::{Cell, CellImmutable};
use super::{DepNode, Watchable};

pub trait TakeExt<T>: Watchable<T> {
    fn take(&self, count: usize) -> Cell<T, CellImmutable>
    where
        T: Clone + Send + Sync + 'static,
        Self: Clone + Send + Sync + 'static,
    {
        let parent: Arc<dyn DepNode> = Arc::new(self.clone());
        let cell = Cell::<T, CellImmutable>::derived(self.get(), vec![parent]);

        let remaining = Arc::new(AtomicUsize::new(count));
        let weak = cell.downgrade();
        let guard = self.subscribe(move |value| {
            if let Some(c) = weak.upgrade() {
                let prev = remaining.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                    if n > 0 { Some(n - 1) } else { None }
                });
                if prev.is_ok() {
                    c.notify(value.clone());
                }
            }
        });
        cell.own(guard);

        cell
    }
}

impl<T, W: Watchable<T>> TakeExt<T> for W {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Mutable;
    use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
    use std::sync::Arc;

    #[test]
    fn test_take_limits_emissions() {
        let source = Cell::new(0u64);
        let taken = source.take(3);
        let count = Arc::new(AtomicU64::new(0));

        let c = count.clone();
        let _guard = taken.subscribe(move |_| {
            c.fetch_add(1, AtomicOrdering::SeqCst);
        });

        // Initial watch call counts as 1
        assert_eq!(count.load(AtomicOrdering::SeqCst), 1);

        source.set(1); // 2
        source.set(2); // 3
        source.set(3); // ignored
        source.set(4); // ignored

        assert_eq!(count.load(AtomicOrdering::SeqCst), 3);
    }
}
