use std::sync::Arc;
use arc_swap::ArcSwap;
use crate::cell::{Cell, CellImmutable};
use super::{DepNode, Watchable};

pub trait PairwiseExt<T>: Watchable<T> {
    fn pairwise(&self) -> Cell<(T, T), CellImmutable>
    where
        T: Clone + Send + Sync + 'static,
        Self: Clone + Send + Sync + 'static,
    {
        let initial = self.get();
        let parent: Arc<dyn DepNode> = Arc::new(self.clone());
        let cell = Cell::<(T, T), CellImmutable>::derived((initial.clone(), initial.clone()), vec![parent]);

        let prev = Arc::new(ArcSwap::from_pointee(initial));
        let weak = cell.downgrade();
        let guard = self.subscribe(move |value| {
            if let Some(c) = weak.upgrade() {
                let previous = (**prev.load()).clone();
                prev.store(Arc::new(value.clone()));
                c.notify((previous, value.clone()));
            }
        });
        cell.own(guard);

        cell
    }
}

impl<T, W: Watchable<T>> PairwiseExt<T> for W {}
