use crate::cell::{Cell, CellImmutable};
use super::{ScanExt, Watchable};

pub trait PairwiseExt<T>: Watchable<T> {
    /// Emit pairs of (previous, current) values.
    /// Equivalent to `scan` with pair accumulation.
    fn pairwise(&self) -> Cell<(T, T), CellImmutable>
    where
        T: Clone + Send + Sync + 'static,
        Self: Clone + Send + Sync + 'static,
    {
        let initial = self.get();
        self.scan((initial.clone(), initial), |(_prev, current), new| {
            (current.clone(), new.clone())
        })
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
