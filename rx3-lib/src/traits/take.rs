use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use crate::cell::{Cell, CellImmutable};
use super::{DepNode, Watchable};

pub trait TakeExt<T>: Watchable<T> {
    fn take(&self, count: usize) -> Cell<T, CellImmutable>
    where
        T: Clone + Send + Sync + 'static,
    {
        let parent: Arc<dyn DepNode> = Arc::new(self.clone());
        let cell = Cell::<T, CellImmutable>::derived(self.get(), vec![parent]);

        let remaining = Arc::new(AtomicUsize::new(count));
        let c = cell.clone();
        self.watch(move |value| {
            let prev = remaining.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                if n > 0 { Some(n - 1) } else { None }
            });
            if prev.is_ok() {
                c.notify(value.clone());
            }
        });

        cell
    }
}

impl<T, W: Watchable<T>> TakeExt<T> for W {}
