//! `take(n)` operator — emit up to `n` values, then complete.
//!
//! [`Definite`] when source is `Definite`: the synchronous initial emission
//! counts as the first of `n` (matching the historical cell-based behavior),
//! so `take(1)` materializes to a `Cell<T>` holding the source's initial value
//! and immediately complete.

use std::{
    marker::PhantomData,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use super::CellValue;
use crate::{
    pipeline::{
        Definite, Empty, MaterializeDefinite, MaterializeEmpty, Pipeline, PipelineInstall,
        PipelineSeed, Seedness,
    },
    signal::Signal,
    subscription::SubscriptionGuard,
};

/// Pipeline node representing `source.take(n)`.
pub struct TakePipeline<S, T, Sd = Definite> {
    source: S,
    count: usize,
    _t: PhantomData<fn(T)>,
    _sd: PhantomData<fn(Sd)>,
}

impl<S, T, Sd> PipelineInstall<T> for TakePipeline<S, T, Sd>
where
    S: PipelineInstall<T> + Send + Sync + 'static,
    Sd: Seedness,
    T: CellValue,
{
    fn install(&self, callback: Arc<dyn Fn(&Signal<T>) + Send + Sync>) -> SubscriptionGuard {
        let remaining = Arc::new(AtomicUsize::new(self.count));
        let wrapped: Arc<dyn Fn(&Signal<T>) + Send + Sync> = Arc::new(move |signal: &Signal<T>| {
            match signal {
                Signal::Value(_) => {
                    let prev = remaining.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                        if n > 0 { Some(n - 1) } else { None }
                    });
                    match prev {
                        Ok(1) => {
                            callback(signal);
                            callback(&Signal::Complete);
                        }
                        Ok(_) => callback(signal),
                        Err(_) => {
                            // already exhausted
                        }
                    }
                }
                Signal::Complete => callback(&Signal::Complete),
                Signal::Error(e) => callback(&Signal::Error(e.clone())),
            }
        });
        self.source.install(wrapped)
    }
}

impl<S, T> PipelineSeed<T> for TakePipeline<S, T, Definite>
where
    S: PipelineSeed<T>,
    T: CellValue,
{
    fn seed(&self) -> T {
        self.source.seed()
    }
}

#[allow(private_bounds)]
impl<S, T, Sd> Pipeline<T, Sd> for TakePipeline<S, T, Sd>
where
    S: Pipeline<T, Sd>,
    Sd: Seedness,
    T: CellValue,
{
}

impl<S, T> MaterializeDefinite<T> for TakePipeline<S, T, Definite>
where
    S: Pipeline<T, Definite> + PipelineSeed<T>,
    T: CellValue,
{
}

impl<S, T> MaterializeEmpty<T> for TakePipeline<S, T, Empty>
where
    S: Pipeline<T, Empty>,
    T: CellValue,
{
}

#[allow(private_bounds)]
pub trait TakeExt<T: CellValue, S: Seedness>: Pipeline<T, S> {
    #[track_caller]
    fn take(self, count: usize) -> TakePipeline<Self, T, S> {
        TakePipeline {
            source: self,
            count,
            _t: PhantomData,
            _sd: PhantomData,
        }
    }
}

impl<T: CellValue, S: Seedness, P: Pipeline<T, S>> TakeExt<T, S> for P {}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering as AtomicOrdering},
    };

    use super::*;
    use crate::{Cell, MaterializeDefinite, Mutable, traits::Watchable};

    #[test]
    fn test_take_limits_emissions() {
        let source = Cell::new(0u64);
        let taken = source.clone().take(3).materialize();
        let count = Arc::new(AtomicU64::new(0));

        let c = count.clone();
        let _guard = taken.subscribe(move |signal| {
            if let Signal::Value(_) = signal {
                c.fetch_add(1, AtomicOrdering::SeqCst);
            }
        });

        // Initial sync emit counts as the 1st take.
        assert_eq!(count.load(AtomicOrdering::SeqCst), 1);

        source.set(1); // 2nd
        source.set(2); // 3rd, completes
        source.set(3); // ignored
        source.set(4); // ignored

        assert_eq!(count.load(AtomicOrdering::SeqCst), 3);
    }

    #[test]
    fn test_take_completes() {
        let source = Cell::new(0u64);
        let taken = source.clone().take(2).materialize();
        let completed = Arc::new(AtomicBool::new(false));

        let c = completed.clone();
        let _guard = taken.subscribe(move |signal| {
            if let Signal::Complete = signal {
                c.store(true, AtomicOrdering::SeqCst);
            }
        });

        assert!(!taken.is_complete());
        assert!(!completed.load(AtomicOrdering::SeqCst));

        source.set(1); // 2nd emission, completes

        assert!(taken.is_complete());
        assert!(completed.load(AtomicOrdering::SeqCst));
    }
}
