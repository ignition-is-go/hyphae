use std::sync::Arc;
use arc_swap::ArcSwap;
use crate::cell::{Cell, CellImmutable, CellMutable};
use super::Watchable;

pub trait PairwiseExt<T>: Watchable<T> {
    fn pairwise(&self) -> Cell<(T, T), CellImmutable>
    where
        T: Clone + Send + Sync + 'static,
        Self: Clone + Send + Sync + 'static,
    {
        let initial = self.get();
        let cell = Cell::<(T, T), CellMutable>::new((initial.clone(), initial.clone()));

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

        // Propagate source completion
        let weak = cell.downgrade();
        let complete_guard = self.on_complete(move || {
            if let Some(c) = weak.upgrade() {
                c.complete();
            }
        });
        cell.own(complete_guard);

        cell.lock()
    }
}

impl<T, W: Watchable<T>> PairwiseExt<T> for W {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Gettable, Mutable};

    #[test]
    fn test_pairwise_emits_pairs() {
        let source = Cell::new(1u64);
        let pairs = source.pairwise();

        source.set(2);
        assert_eq!(pairs.get(), (1, 2));

        source.set(3);
        assert_eq!(pairs.get(), (2, 3));
    }
}
