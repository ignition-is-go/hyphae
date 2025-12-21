use std::sync::Arc;
use crate::cell::{Cell, CellImmutable};
use super::{DepNode, Watchable};

pub trait TapExt<T>: Watchable<T> {
    fn tap(&self, f: impl Fn(&T) + Send + Sync + 'static) -> Cell<T, CellImmutable>
    where
        T: Clone + Send + Sync + 'static,
    {
        let parent: Arc<dyn DepNode> = Arc::new(self.clone());
        let cell = Cell::<T, CellImmutable>::derived(self.get(), vec![parent]);

        let c = cell.clone();
        self.watch(move |value| {
            f(value);
            c.notify(value.clone());
        });

        cell
    }
}

impl<T, W: Watchable<T>> TapExt<T> for W {}
