use super::{CellValue, DistinctUntilChangedByExt, DistinctUntilChangedByPipeline};
use crate::pipeline::{Pipeline, PipelineSeed, Seedness};

#[allow(private_bounds)]
pub trait DedupedExt<T: CellValue, S: Seedness>: Pipeline<T, S> + PipelineSeed<T> {
    #[track_caller]
    fn deduped(
        self,
    ) -> DistinctUntilChangedByPipeline<Self, T, impl Fn(&T, &T) -> bool + Send + Sync + 'static, S>
    where
        T: PartialEq,
    {
        self.distinct_until_changed_by(|a, b| a == b)
    }
}

impl<T: CellValue, S: Seedness, P: Pipeline<T, S> + PipelineSeed<T>> DedupedExt<T, S> for P {}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    };

    use super::*;
    use crate::{Cell, MaterializeDefinite, Mutable, Signal, traits::Watchable};

    #[test]
    fn test_deduped_blocks_duplicates() {
        let source = Cell::new(1u64);
        let deduped = source.clone().deduped().materialize();
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
        let deduped = source.clone().deduped().materialize();

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
