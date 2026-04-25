//! `filter` operator — pure predicate; chains fuse into one closure on root.
//!
//! # Cold `get()` semantics
//!
//! `Pipeline::get()` on a `FilterPipeline` returns the source value
//! **unfiltered** — there is no prior value to fall back on when the current
//! source value would fail the predicate. Callers who want "last-seen value
//! that passed" semantics must materialize: the materialized cell caches the
//! last emitted value and only updates when the predicate passes.

use std::{marker::PhantomData, sync::Arc};

use super::CellValue;
use crate::{
    pipeline::{Pipeline, PipelineInstall},
    signal::Signal,
    subscription::SubscriptionGuard,
    traits::Gettable,
};

/// Pipeline node representing `source.filter(p)`. Does not allocate a cell.
pub struct FilterPipeline<S, T, P> {
    source: S,
    predicate: Arc<P>,
    _t: PhantomData<fn(T)>,
}

impl<S: Clone, T, P> Clone for FilterPipeline<S, T, P> {
    fn clone(&self) -> Self {
        Self {
            source: self.source.clone(),
            predicate: Arc::clone(&self.predicate),
            _t: PhantomData,
        }
    }
}

impl<S, T, P> Gettable<T> for FilterPipeline<S, T, P>
where
    S: Gettable<T>,
{
    fn get(&self) -> T {
        self.source.get()
    }
}

impl<S, T, P> PipelineInstall<T> for FilterPipeline<S, T, P>
where
    S: PipelineInstall<T> + Clone + Send + Sync + 'static,
    T: CellValue,
    P: Fn(&T) -> bool + Send + Sync + 'static,
{
    fn install(
        &self,
        callback: Arc<dyn Fn(&Signal<T>) + Send + Sync>,
    ) -> SubscriptionGuard {
        let predicate = Arc::clone(&self.predicate);
        let wrapped: Arc<dyn Fn(&Signal<T>) + Send + Sync> =
            Arc::new(move |signal: &Signal<T>| match signal {
                Signal::Value(v) => {
                    if (predicate)(v.as_ref()) {
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
impl<S, T, P> Pipeline<T> for FilterPipeline<S, T, P>
where
    S: Pipeline<T>,
    T: CellValue,
    P: Fn(&T) -> bool + Send + Sync + 'static,
{
}

pub trait FilterExt<T: CellValue>: Pipeline<T> {
    #[track_caller]
    fn filter<P>(&self, predicate: P) -> FilterPipeline<Self, T, P>
    where
        P: Fn(&T) -> bool + Send + Sync + 'static,
    {
        FilterPipeline {
            source: self.clone(),
            predicate: Arc::new(predicate),
            _t: PhantomData,
        }
    }
}

impl<T: CellValue, P: Pipeline<T>> FilterExt<T> for P {}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    };

    use super::*;
    use crate::{Cell, Mutable, traits::Watchable};

    #[test]
    fn test_filter_passes_matching() {
        let source = Cell::new(10u64);
        let evens = source.filter(|x| x % 2 == 0).materialize();
        let received = Arc::new(AtomicU64::new(0));

        let r = received.clone();
        let _guard = evens.subscribe(move |signal| {
            if let Signal::Value(v) = signal {
                r.store(**v, Ordering::SeqCst);
            }
        });

        assert_eq!(received.load(Ordering::SeqCst), 10);

        source.set(4);
        assert_eq!(received.load(Ordering::SeqCst), 4);
    }

    #[test]
    fn test_filter_blocks_non_matching() {
        let source = Cell::new(10u64);
        let evens = source.filter(|x| x % 2 == 0).materialize();
        let received = Arc::new(AtomicU64::new(0));

        let r = received.clone();
        let _guard = evens.subscribe(move |signal| {
            if let Signal::Value(v) = signal {
                r.store(**v, Ordering::SeqCst);
            }
        });

        source.set(3); // odd - should not pass
        assert_eq!(received.load(Ordering::SeqCst), 10); // still 10

        source.set(6); // even - should pass
        assert_eq!(received.load(Ordering::SeqCst), 6);
    }
}
