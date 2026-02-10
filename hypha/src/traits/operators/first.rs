use super::{CellValue, TakeExt, Watchable};
use crate::cell::{Cell, CellImmutable};

pub trait FirstExt<T>: Watchable<T> {
    /// Take only the first value, then complete.
    /// Equivalent to `take(1)`.
    #[track_caller]
    fn first(&self) -> Cell<T, CellImmutable>
    where
        T: CellValue,
        Self: Clone + Send + Sync + 'static,
    {
        self.take(1)
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
