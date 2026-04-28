//! `map` operator — pure transform; chains fuse into one closure on root.

use std::{marker::PhantomData, sync::Arc};

use super::CellValue;
use crate::{
    pipeline::{Definite, Empty, MaterializeDefinite, MaterializeEmpty, Pipeline, PipelineInstall, PipelineSeed, Seedness},
    signal::Signal,
    subscription::SubscriptionGuard,
};

/// Pipeline node representing `source.map(f)`. Does not allocate a cell.
pub struct MapPipeline<S, T, U, F> {
    source: S,
    f: Arc<F>,
    _types: PhantomData<fn(T) -> U>,
}

impl<S, T, U, F> PipelineInstall<U> for MapPipeline<S, T, U, F>
where
    S: PipelineInstall<T> + Send + Sync + 'static,
    T: CellValue,
    U: CellValue,
    F: Fn(&T) -> U + Send + Sync + 'static,
{
    fn install(
        &self,
        callback: Arc<dyn Fn(&Signal<U>) + Send + Sync>,
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

// Seed only exists when the source has a seed (i.e. source is Definite).
impl<S, T, U, F> PipelineSeed<U> for MapPipeline<S, T, U, F>
where
    S: PipelineSeed<T>,
    T: CellValue,
    U: CellValue,
    F: Fn(&T) -> U + Send + Sync + 'static,
{
    fn seed(&self) -> U {
        (self.f)(&self.source.seed())
    }
}

// Map propagates the source's Seedness.
#[allow(private_bounds)]
impl<S, T, U, F, Sd> Pipeline<U, Sd> for MapPipeline<S, T, U, F>
where
    S: Pipeline<T, Sd>,
    Sd: Seedness,
    T: CellValue,
    U: CellValue,
    F: Fn(&T) -> U + Send + Sync + 'static,
{
}

impl<S, T, U, F> MaterializeDefinite<U> for MapPipeline<S, T, U, F>
where
    S: Pipeline<T, Definite> + PipelineSeed<T>,
    T: CellValue,
    U: CellValue,
    F: Fn(&T) -> U + Send + Sync + 'static,
{
}

impl<S, T, U, F> MaterializeEmpty<U> for MapPipeline<S, T, U, F>
where
    S: Pipeline<T, Empty>,
    T: CellValue,
    U: CellValue,
    F: Fn(&T) -> U + Send + Sync + 'static,
{
}

#[allow(private_bounds)]
pub trait MapExt<T: CellValue, S: Seedness>: Pipeline<T, S> {
    #[track_caller]
    fn map<U, F>(self, f: F) -> MapPipeline<Self, T, U, F>
    where
        U: CellValue,
        F: Fn(&T) -> U + Send + Sync + 'static,
    {
        MapPipeline {
            source: self,
            f: Arc::new(f),
            _types: PhantomData,
        }
    }
}

impl<T: CellValue, S: Seedness, P: Pipeline<T, S>> MapExt<T, S> for P {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Cell, Gettable, MaterializeDefinite, Mutable, traits::DepNode};

    #[test]
    fn test_map_transform() {
        let source = Cell::new(5);
        let doubled = source.clone().map(|x| x * 2).materialize();

        assert_eq!(doubled.get(), 10);

        source.set(10);
        assert_eq!(doubled.get(), 20);
    }

    #[test]
    fn test_map_chain() {
        let source = Cell::new(1);
        let result = source
            .clone()
            .map(|x| x + 1)
            .map(|x| x * 2)
            .map(|x| x + 10)
            .materialize();

        assert_eq!(result.get(), 14); // ((1 + 1) * 2) + 10

        source.set(5);
        assert_eq!(result.get(), 22); // ((5 + 1) * 2) + 10
    }

    #[test]
    fn test_map_type_change() {
        let source = Cell::new(42);
        let stringified = source.map(|x| format!("value: {}", x)).materialize();

        assert_eq!(stringified.get(), "value: 42");
    }

    #[test]
    fn test_map_tracks_dependency() {
        let source = Cell::new(1).with_name("source");
        let mapped = source.map(|x| x * 2).materialize();

        assert_eq!(mapped.dependency_count(), 1);
        assert!(mapped.has_dependencies());
    }
}
