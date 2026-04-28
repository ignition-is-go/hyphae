//! `skip_while(p)` operator — drop emissions while predicate returns true.
//!
//! Once the predicate returns `false` for any emission, that emission and all
//! subsequent ones pass through. [`Empty`] seedness because the initial
//! emission may be skipped.

use std::{
    marker::PhantomData,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use super::CellValue;
use crate::{
    pipeline::{Empty, MaterializeEmpty, Pipeline, PipelineInstall, Seedness},
    signal::Signal,
    subscription::SubscriptionGuard,
};

/// Pipeline node representing `source.skip_while(p)`.
pub struct SkipWhilePipeline<S, T, F, Sd = crate::pipeline::Definite> {
    source: S,
    predicate: Arc<F>,
    _t: PhantomData<fn(T)>,
    _sd: PhantomData<fn(Sd)>,
}

impl<S, T, F, Sd> PipelineInstall<T> for SkipWhilePipeline<S, T, F, Sd>
where
    S: PipelineInstall<T> + Send + Sync + 'static,
    Sd: Seedness,
    T: CellValue,
    F: Fn(&T) -> bool + Send + Sync + 'static,
{
    fn install(
        &self,
        callback: Arc<dyn Fn(&Signal<T>) + Send + Sync>,
    ) -> SubscriptionGuard {
        let predicate = Arc::clone(&self.predicate);
        let skipping = Arc::new(AtomicBool::new(true));
        let wrapped: Arc<dyn Fn(&Signal<T>) + Send + Sync> = Arc::new(move |signal: &Signal<T>| {
            match signal {
                Signal::Value(v) => {
                    if skipping.load(Ordering::SeqCst) {
                        if !predicate(v.as_ref()) {
                            skipping.store(false, Ordering::SeqCst);
                            callback(signal);
                        }
                    } else {
                        callback(signal);
                    }
                }
                Signal::Complete => callback(&Signal::Complete),
                Signal::Error(e) => callback(&Signal::Error(e.clone())),
            }
        });
        self.source.install(wrapped)
    }
}

#[allow(private_bounds)]
impl<S, T, F, Sd> Pipeline<T, Empty> for SkipWhilePipeline<S, T, F, Sd>
where
    S: Pipeline<T, Sd>,
    Sd: Seedness,
    T: CellValue,
    F: Fn(&T) -> bool + Send + Sync + 'static,
{
}

impl<S, T, F, Sd> MaterializeEmpty<T> for SkipWhilePipeline<S, T, F, Sd>
where
    S: Pipeline<T, Sd>,
    Sd: Seedness,
    T: CellValue,
    F: Fn(&T) -> bool + Send + Sync + 'static,
{
}

#[allow(private_bounds)]
pub trait SkipWhileExt<T: CellValue, S: Seedness>: Pipeline<T, S> {
    /// Skip emissions while the predicate returns `true`.
    ///
    /// Once the predicate returns `false`, that value and all subsequent
    /// values pass through.
    ///
    /// # Example
    ///
    /// ```
    /// use hyphae::{Cell, MaterializeEmpty, Mutable, SkipWhileExt};
    ///
    /// let source = Cell::new(0);
    /// let skipped = source.clone().skip_while(|v| *v < 3).materialize();
    ///
    /// source.set(1); // skipped
    /// source.set(2); // skipped
    /// source.set(3); // passes (predicate false)
    /// source.set(1); // passes (gate already opened)
    /// ```
    #[track_caller]
    fn skip_while<F>(self, predicate: F) -> SkipWhilePipeline<Self, T, F, S>
    where
        F: Fn(&T) -> bool + Send + Sync + 'static,
    {
        SkipWhilePipeline {
            source: self,
            predicate: Arc::new(predicate),
            _t: PhantomData,
            _sd: PhantomData,
        }
    }
}

impl<T: CellValue, S: Seedness, P: Pipeline<T, S>> SkipWhileExt<T, S> for P {}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};

    use super::*;
    use crate::{Cell, MaterializeEmpty, Mutable, traits::Watchable};

    #[test]
    fn test_skip_while() {
        let source = Cell::new(0);
        let skipped = source.clone().skip_while(|v| *v < 3).materialize();

        let count = Arc::new(AtomicU32::new(0));
        let c = count.clone();
        let _guard = skipped.subscribe(move |signal| {
            if let Signal::Value(_) = signal {
                c.fetch_add(1, AtomicOrdering::SeqCst);
            }
        });

        // Subscribe fires once with the cell's initial None.
        assert_eq!(count.load(AtomicOrdering::SeqCst), 1);

        source.set(1); // skipped
        assert_eq!(count.load(AtomicOrdering::SeqCst), 1);

        source.set(2); // skipped
        assert_eq!(count.load(AtomicOrdering::SeqCst), 1);

        source.set(3); // passes — gate opens
        assert_eq!(count.load(AtomicOrdering::SeqCst), 2);

        source.set(1); // also passes (gate stays open)
        assert_eq!(count.load(AtomicOrdering::SeqCst), 3);
    }
}
