//! `skip(n)` operator — drop the first `n` source emissions.
//!
//! [`Empty`] seedness: the synchronous-on-subscribe initial emission counts as
//! one of the skips, so the materialized cell starts `None` and stays `None`
//! until the source has emitted `n + 1` values.

use std::{
    marker::PhantomData,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

use super::CellValue;
use crate::{
    pipeline::{Empty, MaterializeEmpty, Pipeline, PipelineInstall, Seedness},
    signal::Signal,
    subscription::SubscriptionGuard,
};

/// Pipeline node representing `source.skip(n)`.
pub struct SkipPipeline<S, T, Sd = crate::pipeline::Definite> {
    source: S,
    count: usize,
    _t: PhantomData<fn(T)>,
    _sd: PhantomData<fn(Sd)>,
}

impl<S, T, Sd> PipelineInstall<T> for SkipPipeline<S, T, Sd>
where
    S: PipelineInstall<T> + Send + Sync + 'static,
    Sd: Seedness,
    T: CellValue,
{
    fn install(&self, callback: Arc<dyn Fn(&Signal<T>) + Send + Sync>) -> SubscriptionGuard {
        let to_skip = Arc::new(AtomicUsize::new(self.count));
        let wrapped: Arc<dyn Fn(&Signal<T>) + Send + Sync> =
            Arc::new(move |signal: &Signal<T>| match signal {
                Signal::Value(_) => {
                    let prev = to_skip.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                        if n > 0 { Some(n - 1) } else { None }
                    });
                    if prev.is_err() {
                        callback(signal);
                    }
                }
                Signal::Complete => callback(&Signal::Complete),
                Signal::Error(e) => callback(&Signal::Error(e.clone())),
            });
        self.source.install(wrapped)
    }
}

#[allow(private_bounds)]
impl<S, T, Sd> Pipeline<T, Empty> for SkipPipeline<S, T, Sd>
where
    S: Pipeline<T, Sd>,
    Sd: Seedness,
    T: CellValue,
{
}

impl<S, T, Sd> MaterializeEmpty<T> for SkipPipeline<S, T, Sd>
where
    S: Pipeline<T, Sd>,
    Sd: Seedness,
    T: CellValue,
{
}

#[allow(private_bounds)]
pub trait SkipExt<T: CellValue, S: Seedness>: Pipeline<T, S> {
    #[track_caller]
    fn skip(self, count: usize) -> SkipPipeline<Self, T, S> {
        SkipPipeline {
            source: self,
            count,
            _t: PhantomData,
            _sd: PhantomData,
        }
    }
}

impl<T: CellValue, S: Seedness, P: Pipeline<T, S>> SkipExt<T, S> for P {}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicU64, Ordering as AtomicOrdering},
    };

    use super::*;
    use crate::{Cell, MaterializeEmpty, Mutable, traits::Watchable};

    #[test]
    fn test_skip_ignores_first_n() {
        let source = Cell::new(0u64);
        let skipped = source.clone().skip(2).materialize();
        let count = Arc::new(AtomicU64::new(0));

        let c = count.clone();
        let _guard = skipped.subscribe(move |signal| {
            if let Signal::Value(_) = signal {
                c.fetch_add(1, AtomicOrdering::SeqCst);
            }
        });

        // Subscribe fires once with the cell's current value (None at this point).
        assert_eq!(count.load(AtomicOrdering::SeqCst), 1);

        // First set: counts as 2nd of N=2 skips (initial sync emit was the 1st).
        source.set(1);
        assert_eq!(count.load(AtomicOrdering::SeqCst), 1);

        // Second set: emits — count moves, cell is now Some(2).
        source.set(2);
        assert_eq!(count.load(AtomicOrdering::SeqCst), 2);

        source.set(3);
        assert_eq!(count.load(AtomicOrdering::SeqCst), 3);
    }
}
