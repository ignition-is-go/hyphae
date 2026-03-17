use super::{CellValue, DistinctUntilChangedByExt, Watchable};
use crate::cell::{Cell, CellImmutable};

pub trait DedupedExt<T>: Watchable<T> {
    #[track_caller]
    fn deduped(&self) -> Cell<T, CellImmutable>
    where
        T: CellValue + PartialEq,
        Self: Clone + Send + Sync + 'static,
    {
        self.distinct_until_changed_by(|a, b| a == b)
    }
}

impl<T, W: Watchable<T>> DedupedExt<T> for W {}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    };

    use super::*;
    use crate::{Mutable, Signal};

    #[test]
    fn test_deduped_blocks_duplicates() {
        let source = Cell::new(1u64);
        let deduped = source.deduped();
        let count = Arc::new(AtomicU64::new(0));

        let c = count.clone();
        let _guard = deduped.subscribe(move |_| {
            c.fetch_add(1, Ordering::SeqCst);
        });

        assert_eq!(count.load(Ordering::SeqCst), 1); // initial

        source.set(1); // same value - blocked
        assert_eq!(count.load(Ordering::SeqCst), 1);

        source.set(2); // different - passes
        assert_eq!(count.load(Ordering::SeqCst), 2);

        source.set(2); // same - blocked
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn test_deduped_propagates_completion() {
        let source = Cell::new(0);
        let deduped = source.deduped();

        let completed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let c = completed.clone();
        let _guard = deduped.subscribe(move |signal| {
            if let Signal::Complete = signal {
                c.store(true, Ordering::SeqCst);
            }
        });

        source.complete();
        assert!(completed.load(Ordering::SeqCst));
    }
}
