use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use arc_swap::ArcSwap;
use crate::cell::{Cell, CellImmutable};
use super::{DepNode, Watchable};

pub trait ScanExt<T>: Watchable<T> {
    fn scan<U, F>(&self, initial: U, f: F) -> Cell<U, CellImmutable>
    where
        T: Clone + Send + Sync + 'static,
        U: Clone + Send + Sync + 'static,
        F: Fn(&U, &T) -> U + Send + Sync + 'static,
        Self: Clone + Send + Sync + 'static,
    {
        let parent: Arc<dyn DepNode> = Arc::new(self.clone());
        let first_acc = f(&initial, &self.get());
        let cell = Cell::<U, CellImmutable>::derived(first_acc.clone(), vec![parent]);

        let acc = Arc::new(ArcSwap::from_pointee(first_acc));
        let weak = cell.downgrade();
        let first = Arc::new(AtomicBool::new(true));
        let guard = self.subscribe(move |value| {
            if first.swap(false, Ordering::SeqCst) {
                return;
            }
            if let Some(c) = weak.upgrade() {
                let current = (**acc.load()).clone();
                let next = f(&current, value);
                acc.store(Arc::new(next.clone()));
                c.notify(next);
            }
        });
        cell.own(guard);

        cell
    }
}

impl<T, W: Watchable<T>> ScanExt<T> for W {}
