//! `filter` operator — pure predicate; chains fuse into one closure on root.
//!
//! # Empty seedness
//!
//! Filter's source initial value may not satisfy the predicate, so there is no
//! honest seed to give the materialized cell. Filter therefore forces
//! `Seedness = Empty`: `cell.filter(p).materialize()` returns
//! `Cell<Option<T>, CellImmutable>`, initialized to `None`.
//!
//! Once the predicate is satisfied for the first time, the cell flips to
//! `Some(value)` and stays `Some(_)` from then on — failing emissions are
//! swallowed at the install boundary and never reach the cell, so they cannot
//! revert it to `None`.

use std::{marker::PhantomData, sync::Arc};

use super::CellValue;
use crate::{
    pipeline::{Definite, Empty, MaterializeEmpty, Pipeline, PipelineInstall, Seedness},
    signal::Signal,
    subscription::SubscriptionGuard,
};

/// Pipeline node representing `source.filter(p)`. Does not allocate a cell.
///
/// The `Sd` parameter records the source's seedness so the trait impls below
/// can bind it without falling foul of E0207. The output seedness is always
/// [`Empty`] regardless of source.
pub struct FilterPipeline<S, T, P, Sd = Definite> {
    source: S,
    predicate: Arc<P>,
    _t: PhantomData<fn(T)>,
    _sd: PhantomData<fn(Sd)>,
}

impl<S, T, P, Sd> PipelineInstall<T> for FilterPipeline<S, T, P, Sd>
where
    S: PipelineInstall<T> + Send + Sync + 'static,
    Sd: Seedness,
    T: CellValue,
    P: Fn(&T) -> bool + Send + Sync + 'static,
{
    fn install(&self, callback: Arc<dyn Fn(&Signal<T>) + Send + Sync>) -> SubscriptionGuard {
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
impl<S, T, P, Sd> Pipeline<T, Empty> for FilterPipeline<S, T, P, Sd>
where
    S: Pipeline<T, Sd>,
    Sd: Seedness,
    T: CellValue,
    P: Fn(&T) -> bool + Send + Sync + 'static,
{
}

impl<S, T, P, Sd> MaterializeEmpty<T> for FilterPipeline<S, T, P, Sd>
where
    S: Pipeline<T, Sd>,
    Sd: Seedness,
    T: CellValue,
    P: Fn(&T) -> bool + Send + Sync + 'static,
{
}

#[allow(private_bounds)]
pub trait FilterExt<T: CellValue, S: Seedness>: Pipeline<T, S> {
    #[track_caller]
    fn filter<P>(self, predicate: P) -> FilterPipeline<Self, T, P, S>
    where
        P: Fn(&T) -> bool + Send + Sync + 'static,
    {
        FilterPipeline {
            source: self,
            predicate: Arc::new(predicate),
            _t: PhantomData,
            _sd: PhantomData,
        }
    }
}

impl<T: CellValue, S: Seedness, P: Pipeline<T, S>> FilterExt<T, S> for P {}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    };

    use super::*;
    use crate::{Cell, Gettable, MaterializeEmpty, Mutable, traits::Watchable};

    #[test]
    fn test_filter_passes_matching() {
        // Initial 10 passes is_even, so cell starts Some(10).
        let source = Cell::new(10u64);
        let evens = source.clone().filter(|x| x % 2 == 0).materialize();
        let received = Arc::new(AtomicU64::new(0));

        let r = received.clone();
        let _guard = evens.subscribe(move |signal| {
            if let Signal::Value(v) = signal {
                if let Some(x) = v.as_ref() {
                    r.store(*x, Ordering::SeqCst);
                }
            }
        });

        assert_eq!(received.load(Ordering::SeqCst), 10);

        source.set(4);
        assert_eq!(received.load(Ordering::SeqCst), 4);
    }

    #[test]
    fn test_filter_blocks_non_matching() {
        let source = Cell::new(10u64);
        let evens = source.clone().filter(|x| x % 2 == 0).materialize();
        let received = Arc::new(AtomicU64::new(0));

        let r = received.clone();
        let _guard = evens.subscribe(move |signal| {
            if let Signal::Value(v) = signal {
                if let Some(x) = v.as_ref() {
                    r.store(*x, Ordering::SeqCst);
                }
            }
        });

        source.set(3); // odd - should not pass
        assert_eq!(received.load(Ordering::SeqCst), 10); // still 10

        source.set(6); // even - should pass
        assert_eq!(received.load(Ordering::SeqCst), 6);
    }

    #[test]
    fn test_filter_initial_failing_predicate_is_none() {
        // Initial 11 fails is_even — cell must start None.
        let source = Cell::new(11u64);
        let evens = source.clone().filter(|x| x % 2 == 0).materialize();

        assert_eq!(evens.get(), None);

        source.set(4);
        assert_eq!(evens.get(), Some(4));

        source.set(7); // odd, swallowed; must NOT revert to None
        assert_eq!(evens.get(), Some(4));
    }
}
