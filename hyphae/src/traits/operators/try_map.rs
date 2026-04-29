//! `try_map` operator — fallible transform; chains fuse into one closure on root.

use std::{marker::PhantomData, sync::Arc};

use super::CellValue;
use crate::{
    pipeline::{
        Definite, Empty, MaterializeDefinite, MaterializeEmpty, Pipeline, PipelineInstall,
        PipelineSeed, Seedness,
    },
    signal::Signal,
    subscription::SubscriptionGuard,
};

/// Pipeline node representing `source.try_map(f)`. Does not allocate a cell.
pub struct TryMapPipeline<S, T, U, E, F> {
    source: S,
    f: Arc<F>,
    _types: PhantomData<fn(T) -> Result<U, E>>,
}

impl<S, T, U, E, F> PipelineInstall<Result<U, E>> for TryMapPipeline<S, T, U, E, F>
where
    S: PipelineInstall<T> + Send + Sync + 'static,
    T: CellValue,
    U: CellValue,
    E: CellValue,
    F: Fn(&T) -> Result<U, E> + Send + Sync + 'static,
{
    fn install(
        &self,
        callback: Arc<dyn Fn(&Signal<Result<U, E>>) + Send + Sync>,
    ) -> SubscriptionGuard {
        let f = Arc::clone(&self.f);
        let wrapped: Arc<dyn Fn(&Signal<T>) + Send + Sync> =
            Arc::new(move |signal: &Signal<T>| match signal {
                Signal::Value(v) => callback(&Signal::value((f)(v.as_ref()))),
                Signal::Complete => callback(&Signal::Complete),
                Signal::Error(e) => callback(&Signal::Error(e.clone())),
            });
        self.source.install(wrapped)
    }
}

impl<S, T, U, E, F> PipelineSeed<Result<U, E>> for TryMapPipeline<S, T, U, E, F>
where
    S: PipelineSeed<T>,
    T: CellValue,
    U: CellValue,
    E: CellValue,
    F: Fn(&T) -> Result<U, E> + Send + Sync + 'static,
{
    fn seed(&self) -> Result<U, E> {
        (self.f)(&self.source.seed())
    }
}

#[allow(private_bounds)]
impl<S, T, U, E, F, Sd> Pipeline<Result<U, E>, Sd> for TryMapPipeline<S, T, U, E, F>
where
    S: Pipeline<T, Sd>,
    Sd: Seedness,
    T: CellValue,
    U: CellValue,
    E: CellValue,
    F: Fn(&T) -> Result<U, E> + Send + Sync + 'static,
{
}

impl<S, T, U, E, F> MaterializeDefinite<Result<U, E>> for TryMapPipeline<S, T, U, E, F>
where
    S: Pipeline<T, Definite> + PipelineSeed<T>,
    T: CellValue,
    U: CellValue,
    E: CellValue,
    F: Fn(&T) -> Result<U, E> + Send + Sync + 'static,
{
}

impl<S, T, U, E, F> MaterializeEmpty<Result<U, E>> for TryMapPipeline<S, T, U, E, F>
where
    S: Pipeline<T, Empty>,
    T: CellValue,
    U: CellValue,
    E: CellValue,
    F: Fn(&T) -> Result<U, E> + Send + Sync + 'static,
{
}

/// Extension trait for fallible transformations.
#[allow(private_bounds)]
pub trait TryMapExt<T: CellValue, S: Seedness>: Pipeline<T, S> {
    /// Transform values with a fallible function.
    ///
    /// Returns a [`TryMapPipeline`] node that yields `Ok(value)` when the
    /// transform succeeds, or `Err(error)` when it fails. Materialize to
    /// observe.
    #[track_caller]
    fn try_map<U, E, F>(self, f: F) -> TryMapPipeline<Self, T, U, E, F>
    where
        U: CellValue,
        E: CellValue,
        F: Fn(&T) -> Result<U, E> + Send + Sync + 'static,
    {
        TryMapPipeline {
            source: self,
            f: Arc::new(f),
            _types: PhantomData,
        }
    }
}

impl<T: CellValue, S: Seedness, P: Pipeline<T, S>> TryMapExt<T, S> for P {}
