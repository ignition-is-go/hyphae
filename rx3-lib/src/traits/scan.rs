use std::sync::Arc;
use arc_swap::ArcSwap;
use crate::cell::{Cell, CellImmutable};
use super::{DepNode, Watchable};

pub trait ScanExt<T>: Watchable<T> {
    fn scan<U, F>(&self, initial: U, f: F) -> Cell<U, CellImmutable>
    where
        T: Clone + Send + Sync + 'static,
        U: Clone + Send + Sync + 'static,
        F: Fn(&U, &T) -> U + Send + Sync + 'static,
    {
        let parent: Arc<dyn DepNode> = Arc::new(self.clone());
        let cell = Cell::<U, CellImmutable>::derived(initial.clone(), vec![parent]);

        let acc = Arc::new(ArcSwap::from_pointee(initial));
        let c = cell.clone();
        self.watch(move |value| {
            let current = (**acc.load()).clone();
            let next = f(&current, value);
            acc.store(Arc::new(next.clone()));
            c.notify(next);
        });

        cell
    }
}

impl<T, W: Watchable<T>> ScanExt<T> for W {}
