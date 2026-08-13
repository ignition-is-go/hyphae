//! `tap` operator — pure side effect; chains fuse into one closure on root.

use std::{marker::PhantomData, sync::Arc};

use super::CellValue;
use crate::{
    pipeline::{Pipeline, PipelineInstall, PipelineSeed, Seedness},
    signal::Signal,
    subscription::SubscriptionGuard,
};

/// Pipeline node representing `source.tap(f)`. Does not allocate a cell.
pub struct TapPipeline<S, T, F> {
    source: S,
    f: Arc<F>,
    _t: PhantomData<fn(T)>,
}

impl<S, T, F> PipelineInstall<T> for TapPipeline<S, T, F>
where
    S: PipelineInstall<T> + Send + Sync + 'static,
    T: CellValue,
    F: Fn(&T) + Send + Sync + 'static,
{
    fn install(&self, callback: Arc<dyn Fn(&Signal<T>) + Send + Sync>) -> SubscriptionGuard {
        let f = Arc::clone(&self.f);
        let wrapped: Arc<dyn Fn(&Signal<T>) + Send + Sync> = Arc::new(move |signal: &Signal<T>| {
            if let Signal::Value(v) = signal {
                (f)(v.as_ref());
            }
            callback(signal);
        });
        self.source.install(wrapped)
    }
}

impl<S, T, F> PipelineSeed<T> for TapPipeline<S, T, F>
where
    S: Pipeline<T, crate::pipeline::Definite>,
    T: CellValue,
    F: Fn(&T) + Send + Sync + 'static,
{
    fn seed(&self) -> T {
        let v = self.source.pipeline_seed();
        (self.f)(&v);
        v
    }
}

#[allow(private_bounds)]
impl<S, T, F> Pipeline<T, crate::pipeline::Definite> for TapPipeline<S, T, F>
where
    S: Pipeline<T, crate::pipeline::Definite>,
    T: CellValue,
    F: Fn(&T) + Send + Sync + 'static,
{
}

impl<S, T, F> Pipeline<T, crate::pipeline::Empty> for TapPipeline<S, T, F>
where
    S: Pipeline<T, crate::pipeline::Empty>,
    T: CellValue,
    F: Fn(&T) + Send + Sync + 'static,
{
}

/// Extension trait for side-effecting observation.
#[allow(private_bounds)]
pub trait TapExt<T: CellValue, S: Seedness>: Pipeline<T, S> {
    /// Run `f(&value)` for side effects and forward the value untransformed.
    ///
    /// Returns an opaque lazy pipeline. Materialize to observe.
    #[track_caller]
    fn tap<F>(self, f: F) -> impl crate::Materialize<T, S>
    where
        F: Fn(&T) + Send + Sync + 'static;
}

impl<T: CellValue, P: Pipeline<T, crate::pipeline::Definite>> TapExt<T, crate::pipeline::Definite>
    for P
{
    fn tap<F>(self, f: F) -> impl crate::Materialize<T, crate::pipeline::Definite>
    where
        F: Fn(&T) + Send + Sync + 'static,
    {
        TapPipeline {
            source: self,
            f: Arc::new(f),
            _t: PhantomData,
        }
    }
}

impl<T: CellValue, P: Pipeline<T, crate::pipeline::Empty>> TapExt<T, crate::pipeline::Empty> for P {
    fn tap<F>(self, f: F) -> impl crate::Materialize<T, crate::pipeline::Empty>
    where
        F: Fn(&T) + Send + Sync + 'static,
    {
        TapPipeline {
            source: self,
            f: Arc::new(f),
            _t: PhantomData,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    };

    use super::*;
    use crate::{Cell, Gettable, Materialize, Mutable};

    #[test]
    fn test_tap_side_effect() {
        let source = Cell::new(0u64);
        let side_effect = Arc::new(AtomicU64::new(0));

        let se = side_effect.clone();
        let tapped = source
            .clone()
            .tap(move |v| {
                se.store(*v, Ordering::SeqCst);
            })
            .materialize();

        source.set(42);
        assert_eq!(side_effect.load(Ordering::SeqCst), 42);
        assert_eq!(tapped.get(), 42); // value passes through unchanged
    }
}
