//! `try_map` operator — fallible transform; chains fuse into one closure on root.

use std::{marker::PhantomData, sync::Arc};

use super::CellValue;
use crate::{
    pipeline::{Emit, EmitSink, Pipeline, PipelineInstall, PipelineSeed, Seedness},
    signal::Signal,
    subscription::SubscriptionGuard,
};

/// Pipeline node representing `source.try_map(f)`. Does not allocate a cell.
pub struct TryMapPipeline<S, T, F> {
    source: S,
    f: Arc<F>,
    _types: PhantomData<fn(T)>,
}

impl<S, T, U, E, F> PipelineInstall<Result<U, E>> for TryMapPipeline<S, T, F>
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
        self.install_emit(Arc::new(move |emit| callback(&emit.into_signal())))
    }

    fn install_emit(&self, sink: EmitSink<Result<U, E>>) -> SubscriptionGuard {
        let f = Arc::clone(&self.f);
        self.source.install_emit(Arc::new(move |emit| match emit {
            Emit::Shared(v) => sink(Emit::Owned((f)(v))),
            Emit::Owned(v) => sink(Emit::Owned((f)(&v))),
            Emit::Complete => sink(Emit::Complete),
            Emit::Error(e) => sink(Emit::Error(e)),
        }))
    }
}

impl<S, T, U, E, F> PipelineSeed<Result<U, E>> for TryMapPipeline<S, T, F>
where
    S: Pipeline<T, crate::pipeline::Definite>,
    T: CellValue,
    U: CellValue,
    E: CellValue,
    F: Fn(&T) -> Result<U, E> + Send + Sync + 'static,
{
    fn seed(&self) -> Result<U, E> {
        (self.f)(&self.source.pipeline_seed())
    }
}

#[allow(private_bounds)]
impl<S, T, U, E, F> Pipeline<Result<U, E>, crate::pipeline::Definite> for TryMapPipeline<S, T, F>
where
    S: Pipeline<T, crate::pipeline::Definite>,
    T: CellValue,
    U: CellValue,
    E: CellValue,
    F: Fn(&T) -> Result<U, E> + Send + Sync + 'static,
{
}

impl<S, T, U, E, F> Pipeline<Result<U, E>, crate::pipeline::Empty> for TryMapPipeline<S, T, F>
where
    S: Pipeline<T, crate::pipeline::Empty>,
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
    /// Returns an opaque lazy pipeline that yields `Ok(value)` when the
    /// transform succeeds, or `Err(error)` when it fails. Materialize to
    /// observe.
    fn try_map<U, E, F>(self, f: F) -> impl crate::Materialize<Result<U, E>, S>
    where
        U: CellValue,
        E: CellValue,
        F: Fn(&T) -> Result<U, E> + Send + Sync + 'static;
}

impl<T: CellValue, P: Pipeline<T, crate::pipeline::Definite>>
    TryMapExt<T, crate::pipeline::Definite> for P
{
    fn try_map<U, E, F>(
        self,
        f: F,
    ) -> impl crate::Materialize<Result<U, E>, crate::pipeline::Definite>
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

impl<T: CellValue, P: Pipeline<T, crate::pipeline::Empty>> TryMapExt<T, crate::pipeline::Empty>
    for P
{
    fn try_map<U, E, F>(self, f: F) -> impl crate::Materialize<Result<U, E>, crate::pipeline::Empty>
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
