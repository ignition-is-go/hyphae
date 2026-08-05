//! `scan(initial, f)` operator — fold over each emission, emitting the new accumulator.
//!
//! [`Definite`] when source is `Definite`. The seed of the materialized cell is
//! `f(initial, source.seed())` (the accumulator after one source emission), so
//! `scan(0, +).materialize().get()` on `Cell::new(1)` returns `1`.

use parking_lot::Mutex;
use std::{marker::PhantomData, sync::Arc};

use super::CellValue;
use crate::{
    pipeline::{Definite, Pipeline, PipelineInstall, PipelineSeed, Seedness},
    signal::Signal,
    subscription::SubscriptionGuard,
};

/// Pipeline node representing `source.scan(initial, f)`.
pub struct ScanPipeline<S, T, U, F, Sd = Definite> {
    source: S,
    initial: U,
    f: Arc<F>,
    _t: PhantomData<fn(T)>,
    _sd: PhantomData<fn(Sd)>,
}

impl<S, T, U, F, Sd> PipelineInstall<U> for ScanPipeline<S, T, U, F, Sd>
where
    S: PipelineInstall<T> + Send + Sync + 'static,
    Sd: Seedness,
    T: CellValue,
    U: CellValue,
    F: Fn(&U, &T) -> U + Send + Sync + 'static,
{
    fn install(&self, callback: Arc<dyn Fn(&Signal<U>) + Send + Sync>) -> SubscriptionGuard {
        let f = Arc::clone(&self.f);
        // Capture a fresh accumulator seeded with `initial`. The first
        // emission (the synchronous initial replay) advances it to first_acc,
        // which matches the cell's seed.
        let acc: Arc<Mutex<U>> = Arc::new(Mutex::new(self.initial.clone()));
        let wrapped: Arc<dyn Fn(&Signal<T>) + Send + Sync> =
            Arc::new(move |signal: &Signal<T>| match signal {
                Signal::Value(v) => {
                    let next = {
                        let mut guard = acc.lock();
                        let next = f(&*guard, v.as_ref());
                        *guard = next.clone();
                        next
                    };
                    callback(&Signal::value(next));
                }
                Signal::Complete => callback(&Signal::Complete),
                Signal::Error(e) => callback(&Signal::Error(e.clone())),
            });
        self.source.install(wrapped)
    }
}

impl<S, T, U, F, Sd> PipelineSeed<U> for ScanPipeline<S, T, U, F, Sd>
where
    S: Pipeline<T, crate::pipeline::Definite>,
    Sd: Seedness,
    T: CellValue,
    U: CellValue,
    F: Fn(&U, &T) -> U + Send + Sync + 'static,
{
    fn seed(&self) -> U {
        (self.f)(&self.initial, &self.source.pipeline_seed())
    }
}

#[allow(private_bounds)]
impl<S, T, U, F> Pipeline<U, crate::pipeline::Definite>
    for ScanPipeline<S, T, U, F, crate::pipeline::Definite>
where
    S: Pipeline<T, crate::pipeline::Definite>,
    T: CellValue,
    U: CellValue,
    F: Fn(&U, &T) -> U + Send + Sync + 'static,
{
}

impl<S, T, U, F> Pipeline<U, crate::pipeline::Empty>
    for ScanPipeline<S, T, U, F, crate::pipeline::Empty>
where
    S: Pipeline<T, crate::pipeline::Empty>,
    T: CellValue,
    U: CellValue,
    F: Fn(&U, &T) -> U + Send + Sync + 'static,
{
}

#[allow(private_bounds)]
pub trait ScanExt<T: CellValue, S: Seedness>: Pipeline<T, S> {
    #[track_caller]
    fn scan<U, F>(self, initial: U, f: F) -> impl crate::Materialize<U, S>
    where
        U: CellValue,
        F: Fn(&U, &T) -> U + Send + Sync + 'static;
}

impl<T: CellValue, P: Pipeline<T, crate::pipeline::Definite>> ScanExt<T, crate::pipeline::Definite>
    for P
{
    fn scan<U, F>(self, initial: U, f: F) -> impl crate::Materialize<U, crate::pipeline::Definite>
    where
        U: CellValue,
        F: Fn(&U, &T) -> U + Send + Sync + 'static,
    {
        ScanPipeline {
            source: self,
            initial,
            f: Arc::new(f),
            _t: PhantomData,
            _sd: PhantomData,
        }
    }
}

impl<T: CellValue, P: Pipeline<T, crate::pipeline::Empty>> ScanExt<T, crate::pipeline::Empty>
    for P
{
    fn scan<U, F>(self, initial: U, f: F) -> impl crate::Materialize<U, crate::pipeline::Empty>
    where
        U: CellValue,
        F: Fn(&U, &T) -> U + Send + Sync + 'static,
    {
        ScanPipeline {
            source: self,
            initial,
            f: Arc::new(f),
            _t: PhantomData,
            _sd: PhantomData,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Cell, Gettable, Materialize, Mutable};

    #[test]
    fn test_scan_accumulates() {
        let source = Cell::new(1u64);
        let sum = source.clone().scan(0u64, |acc, x| acc + x).materialize();

        // Initial: 0 + 1 = 1
        assert_eq!(sum.get(), 1);

        source.set(2);
        assert_eq!(sum.get(), 3); // 1 + 2

        source.set(3);
        assert_eq!(sum.get(), 6); // 3 + 3
    }

    #[test]
    fn test_scan_with_different_types() {
        let source = Cell::new(1);
        let collected = source
            .clone()
            .scan(String::new(), |acc, x| format!("{acc}{x}"))
            .materialize();

        assert_eq!(collected.get(), "1");

        source.set(2);
        assert_eq!(collected.get(), "12");

        source.set(3);
        assert_eq!(collected.get(), "123");
    }
}
