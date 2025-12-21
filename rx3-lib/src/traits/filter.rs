use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use crate::cell::{Cell, CellImmutable};
use super::{DepNode, Watchable};

pub trait FilterExt<T>: Watchable<T> {
    fn filter(&self, predicate: impl Fn(&T) -> bool + Send + Sync + 'static) -> Cell<T, CellImmutable>
    where
        T: Clone + Send + Sync + 'static,
    {
        let parent: Arc<dyn DepNode> = Arc::new(self.clone());
        let cell = Cell::<T, CellImmutable>::derived(self.get(), vec![parent]);

        let weak = cell.downgrade();
        let predicate = Arc::new(predicate);
        let first = Arc::new(AtomicBool::new(true));
        self.watch(move |value| {
            if first.swap(false, Ordering::SeqCst) {
                return;
            }
            if let Some(c) = weak.upgrade() {
                if predicate(value) {
                    c.notify(value.clone());
                }
            }
        });

        cell
    }
}

impl<T, W: Watchable<T>> FilterExt<T> for W {}
