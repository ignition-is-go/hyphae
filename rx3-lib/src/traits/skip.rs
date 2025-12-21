use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use crate::cell::{Cell, CellImmutable};
use super::{DepNode, SubscribeExt, Watchable};

pub trait SkipExt<T>: Watchable<T> {
    fn skip(&self, count: usize) -> Cell<T, CellImmutable>
    where
        T: Clone + Send + Sync + 'static,
        Self: Clone + Send + Sync + 'static,
    {
        let parent: Arc<dyn DepNode> = Arc::new(self.clone());
        let cell = Cell::<T, CellImmutable>::derived(self.get(), vec![parent]);

        let to_skip = Arc::new(AtomicUsize::new(count));
        let weak = cell.downgrade();
        let guard = self.subscribe(move |value| {
            if let Some(c) = weak.upgrade() {
                let prev = to_skip.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                    if n > 0 { Some(n - 1) } else { None }
                });
                if prev.is_err() {
                    c.notify(value.clone());
                }
            }
        });
        cell.own(guard);

        cell
    }
}

impl<T, W: Watchable<T>> SkipExt<T> for W {}
