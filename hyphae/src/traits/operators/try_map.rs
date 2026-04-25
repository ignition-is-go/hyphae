//! `try_map` operator — fallible transform; chains fuse into one closure on root.

use std::{marker::PhantomData, sync::Arc};

use super::CellValue;
use crate::{
    pipeline::{Pipeline, PipelineInstall},
    signal::Signal,
    subscription::SubscriptionGuard,
    traits::Gettable,
};

/// Pipeline node representing `source.try_map(f)`. Does not allocate a cell.
pub struct TryMapPipeline<S, T, U, E, F> {
    source: S,
    f: Arc<F>,
    _types: PhantomData<fn(T) -> Result<U, E>>,
}

impl<S: Clone, T, U, E, F> Clone for TryMapPipeline<S, T, U, E, F> {
    fn clone(&self) -> Self {
        Self {
            source: self.source.clone(),
            f: Arc::clone(&self.f),
            _types: PhantomData,
        }
    }
}

impl<S, T, U, E, F> Gettable<Result<U, E>> for TryMapPipeline<S, T, U, E, F>
where
    S: Gettable<T>,
    F: Fn(&T) -> Result<U, E>,
{
    fn get(&self) -> Result<U, E> {
        (self.f)(&self.source.get())
    }
}

impl<S, T, U, E, F> PipelineInstall<Result<U, E>> for TryMapPipeline<S, T, U, E, F>
where
    S: PipelineInstall<T> + Clone + Send + Sync + 'static,
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

#[allow(private_bounds)]
impl<S, T, U, E, F> Pipeline<Result<U, E>> for TryMapPipeline<S, T, U, E, F>
where
    S: Pipeline<T>,
    T: CellValue,
    U: CellValue,
    E: CellValue,
    F: Fn(&T) -> Result<U, E> + Send + Sync + 'static,
{
}

/// Extension trait for fallible transformations.
pub trait TryMapExt<T: CellValue>: Pipeline<T> {
    /// Transform values with a fallible function.
    ///
    /// Returns a [`TryMapPipeline`] node that yields `Ok(value)` when the
    /// transform succeeds, or `Err(error)` when it fails. Materialize to
    /// observe.
    ///
    /// # Example
    /// ```
    /// use hyphae::{Cell, Mutable, TryMapExt, Pipeline, Gettable};
    ///
    /// let source = Cell::new(10i32);
    /// let parsed = source.try_map(|v| {
    ///     if *v > 0 {
    ///         Ok(v.to_string())
    ///     } else {
    ///         Err("must be positive")
    ///     }
    /// }).materialize();
    ///
    /// assert_eq!(parsed.get(), Ok("10".to_string()));
    /// ```
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

impl<T: CellValue, P: Pipeline<T>> TryMapExt<T> for P {}
