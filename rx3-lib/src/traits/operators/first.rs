use std::sync::Arc;
use crate::cell::{Cell, CellImmutable};
use super::{DepNode, Watchable};

pub trait FirstExt<T>: Watchable<T> {
    /// Take only the first value, then complete.
    fn first(&self) -> Cell<T, CellImmutable>
    where
        T: Clone + Send + Sync + 'static,
        Self: Clone + Send + Sync + 'static,
    {
        let parent: Arc<dyn DepNode> = Arc::new(self.clone());
        let derived = Cell::<T, CellImmutable>::derived(self.get(), vec![parent]);

        // Already got first value - complete immediately
        derived.complete();

        derived
    }
}

impl<T, W: Watchable<T>> FirstExt<T> for W {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Gettable, Mutable};

    #[test]
    fn test_first() {
        let source = Cell::new(42);
        let first = source.first();

        assert_eq!(first.get(), 42);
        assert!(first.is_complete()); // Completes immediately

        source.set(100);
        assert_eq!(first.get(), 42); // Still 42, only takes first
    }
}
