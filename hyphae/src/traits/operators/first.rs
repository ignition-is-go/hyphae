use super::{CellValue, TakeExt, TakePipeline};
use crate::pipeline::{Pipeline, Seedness};

#[allow(private_bounds)]
pub trait FirstExt<T: CellValue, S: Seedness>: Pipeline<T, S> {
    /// Take only the first value, then complete. Equivalent to `take(1)`.
    #[track_caller]
    fn first(self) -> TakePipeline<Self, T, S> {
        self.take(1)
    }
}

impl<T: CellValue, S: Seedness, P: Pipeline<T, S>> FirstExt<T, S> for P {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Cell, Gettable, MaterializeDefinite, Mutable, traits::Watchable};

    #[test]
    fn test_first() {
        let source = Cell::new(42);
        let first = source.clone().first().materialize();

        assert_eq!(first.get(), 42);
        assert!(first.is_complete()); // Completes immediately

        source.set(100);
        assert_eq!(first.get(), 42); // Still 42, only takes first
    }
}
