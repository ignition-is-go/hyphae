//! `map` operator — pure transform; chains fuse into one closure on root.

use std::{marker::PhantomData, sync::Arc};

use super::CellValue;
use crate::{
    pipeline::{Pipeline, PipelineInstall},
    signal::Signal,
    subscription::SubscriptionGuard,
    traits::Gettable,
};

/// Pipeline node representing `source.map(f)`. Does not allocate a cell.
pub struct MapPipeline<S, T, U, F> {
    source: S,
    f: Arc<F>,
    _types: PhantomData<fn(T) -> U>,
}

impl<S: Clone, T, U, F> Clone for MapPipeline<S, T, U, F> {
    fn clone(&self) -> Self {
        Self {
            source: self.source.clone(),
            f: Arc::clone(&self.f),
            _types: PhantomData,
        }
    }
}

impl<S, T, U, F> Gettable<U> for MapPipeline<S, T, U, F>
where
    S: Gettable<T>,
    F: Fn(&T) -> U,
{
    fn get(&self) -> U {
        (self.f)(&self.source.get())
    }
}

impl<S, T, U, F> PipelineInstall<U> for MapPipeline<S, T, U, F>
where
    S: PipelineInstall<T> + Clone + Send + Sync + 'static,
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

#[allow(private_bounds)]
impl<S, T, U, F> Pipeline<U> for MapPipeline<S, T, U, F>
where
    S: Pipeline<T>,
    T: CellValue,
    U: CellValue,
    F: Fn(&T) -> U + Send + Sync + 'static,
{
}

pub trait MapExt<T: CellValue>: Pipeline<T> {
    #[track_caller]
    fn map<U, F>(&self, f: F) -> MapPipeline<Self, T, U, F>
    where
        U: CellValue,
        F: Fn(&T) -> U + Send + Sync + 'static,
    {
        MapPipeline {
            source: self.clone(),
            f: Arc::new(f),
            _types: PhantomData,
        }
    }
}

impl<T: CellValue, P: Pipeline<T>> MapExt<T> for P {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Cell, Mutable, traits::DepNode};

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
