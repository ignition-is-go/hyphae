use std::sync::Arc;
use arc_swap::ArcSwap;
use crate::cell::{Cell, CellImmutable};
use super::{DepNode, Watchable};

pub trait PairwiseExt<T>: Watchable<T> {
    fn pairwise(&self) -> Cell<(T, T), CellImmutable>
    where
        T: Clone + Send + Sync + 'static,
    {
        let initial = self.get();
        let parent: Arc<dyn DepNode> = Arc::new(self.clone());
        let cell = Cell::<(T, T), CellImmutable>::derived((initial.clone(), initial.clone()), vec![parent]);

        let prev = Arc::new(ArcSwap::from_pointee(initial));
        let c = cell.clone();
        self.watch(move |value| {
            let previous = (**prev.load()).clone();
            prev.store(Arc::new(value.clone()));
            c.notify((previous, value.clone()));
        });

        cell
    }
}

impl<T, W: Watchable<T>> PairwiseExt<T> for W {}
