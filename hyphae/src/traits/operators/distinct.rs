//! `distinct` operator — drop emissions whose value has been seen before.
//!
//! [`Definite`] when source is `Definite`: the source's initial value is
//! treated as the first "seen" value, so the materialized cell starts at
//! `source.seed()` and only updates on values not previously emitted.

use std::{
    hash::Hash,
    marker::PhantomData,
    sync::Arc,
};

use dashmap::DashSet;

use super::CellValue;
use crate::{
    pipeline::{
        Definite, Empty, MaterializeDefinite, MaterializeEmpty, Pipeline, PipelineInstall,
        PipelineSeed, Seedness,
    },
    signal::Signal,
    subscription::SubscriptionGuard,
};

/// Pipeline node representing `source.distinct()`.
pub struct DistinctPipeline<S, T, Sd = Definite> {
    source: S,
    _t: PhantomData<fn(T)>,
    _sd: PhantomData<fn(Sd)>,
}

impl<S, T, Sd> PipelineInstall<T> for DistinctPipeline<S, T, Sd>
where
    S: PipelineInstall<T> + PipelineSeed<T> + Send + Sync + 'static,
    Sd: Seedness,
    T: CellValue + Eq + Hash,
{
    fn install(
        &self,
        callback: Arc<dyn Fn(&Signal<T>) + Send + Sync>,
    ) -> SubscriptionGuard {
        let seen: Arc<DashSet<T>> = Arc::new(DashSet::new());
        // Pre-insert source's seed so the synchronous initial replay is naturally
        // gated (the cell already holds it).
        seen.insert(self.source.seed());
        let wrapped: Arc<dyn Fn(&Signal<T>) + Send + Sync> = Arc::new(move |signal: &Signal<T>| {
            match signal {
                Signal::Value(v) => {
                    if seen.insert(v.as_ref().clone()) {
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

impl<S, T> PipelineSeed<T> for DistinctPipeline<S, T, Definite>
where
    S: PipelineSeed<T>,
    T: CellValue + Eq + Hash,
{
    fn seed(&self) -> T {
        self.source.seed()
    }
}

#[allow(private_bounds)]
impl<S, T, Sd> Pipeline<T, Sd> for DistinctPipeline<S, T, Sd>
where
    S: Pipeline<T, Sd> + PipelineSeed<T>,
    Sd: Seedness,
    T: CellValue + Eq + Hash,
{
}

impl<S, T> MaterializeDefinite<T> for DistinctPipeline<S, T, Definite>
where
    S: Pipeline<T, Definite> + PipelineSeed<T>,
    T: CellValue + Eq + Hash,
{
}

impl<S, T> MaterializeEmpty<T> for DistinctPipeline<S, T, Empty>
where
    S: Pipeline<T, Empty> + PipelineSeed<T>,
    T: CellValue + Eq + Hash,
{
}

#[allow(private_bounds)]
pub trait DistinctExt<T: CellValue + Eq + Hash, S: Seedness>:
    Pipeline<T, S> + PipelineSeed<T>
{
    /// Filter out values that have already been emitted (by `Hash`/`Eq`).
    ///
    /// # Example
    ///
    /// ```
    /// use hyphae::{Cell, DistinctExt, MaterializeDefinite, Mutable};
    ///
    /// let source = Cell::new(0);
    /// let distinct = source.clone().distinct().materialize();
    ///
    /// source.set(1);
    /// source.set(2);
    /// source.set(1); // blocked - already seen
    /// source.set(3);
    /// source.set(2); // blocked - already seen
    /// ```
    #[track_caller]
    fn distinct(self) -> DistinctPipeline<Self, T, S> {
        DistinctPipeline {
            source: self,
            _t: PhantomData,
            _sd: PhantomData,
        }
    }
}

impl<T: CellValue + Eq + Hash, S: Seedness, P: Pipeline<T, S> + PipelineSeed<T>>
    DistinctExt<T, S> for P
{
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;
    use crate::{Cell, MaterializeDefinite, Mutable, traits::Watchable};

    #[test]
    fn test_distinct() {
        let source = Cell::new(0);
        let distinct = source.clone().distinct().materialize();

        let count = Arc::new(AtomicU32::new(0));
        let c = count.clone();
        let _guard = distinct.subscribe(move |signal| {
            if let Signal::Value(_) = signal {
                c.fetch_add(1, Ordering::SeqCst);
            }
        });

        assert_eq!(count.load(Ordering::SeqCst), 1); // Initial

        source.set(1);
        assert_eq!(count.load(Ordering::SeqCst), 2);

        source.set(2);
        assert_eq!(count.load(Ordering::SeqCst), 3);

        source.set(1); // Already seen
        assert_eq!(count.load(Ordering::SeqCst), 3);

        source.set(3);
        assert_eq!(count.load(Ordering::SeqCst), 4);

        source.set(2); // Already seen
        assert_eq!(count.load(Ordering::SeqCst), 4);
    }
}
