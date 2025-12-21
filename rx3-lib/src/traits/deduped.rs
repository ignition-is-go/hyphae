use std::sync::Arc;
use crate::cell::{Cell, CellImmutable};
use super::{DepNode, Watchable};

pub trait DedupedExt<T>: Watchable<T> {
    fn deduped(&self) -> Cell<T, CellImmutable>
    where
        T: Clone + PartialEq + Send + Sync + 'static,
    {
        let parent: Arc<dyn DepNode> = Arc::new(self.clone());
        let derived = Cell::<T, CellImmutable>::derived(self.get(), vec![parent]);

        let d = derived.clone();
        self.watch(move |value| {
            if *value != d.get() {
                d.notify(value.clone());
            }
        });

        derived
    }
}

impl<T, W: Watchable<T>> DedupedExt<T> for W {}
