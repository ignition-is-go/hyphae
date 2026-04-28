//! `tap` operator — pure side effect; chains fuse into one closure on root.

use std::{marker::PhantomData, sync::Arc};

use super::CellValue;
use crate::{
    pipeline::{Definite, Empty, MaterializeDefinite, MaterializeEmpty, Pipeline, PipelineInstall, PipelineSeed, Seedness},
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
    fn install(
        &self,
        callback: Arc<dyn Fn(&Signal<T>) + Send + Sync>,
    ) -> SubscriptionGuard {
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
    S: PipelineSeed<T>,
    T: CellValue,
    F: Fn(&T) + Send + Sync + 'static,
{
    fn seed(&self) -> T {
        let v = self.source.seed();
        (self.f)(&v);
        v
    }
}

#[allow(private_bounds)]
impl<S, T, F, Sd> Pipeline<T, Sd> for TapPipeline<S, T, F>
where
    S: Pipeline<T, Sd>,
    Sd: Seedness,
    T: CellValue,
    F: Fn(&T) + Send + Sync + 'static,
{
}

impl<S, T, F> MaterializeDefinite<T> for TapPipeline<S, T, F>
where
    S: Pipeline<T, Definite> + PipelineSeed<T>,
    T: CellValue,
    F: Fn(&T) + Send + Sync + 'static,
{
}

impl<S, T, F> MaterializeEmpty<T> for TapPipeline<S, T, F>
where
    S: Pipeline<T, Empty>,
    T: CellValue,
    F: Fn(&T) + Send + Sync + 'static,
{
}

/// Extension trait for side-effecting observation.
#[allow(private_bounds)]
pub trait TapExt<T: CellValue, S: Seedness>: Pipeline<T, S> {
    /// Run `f(&value)` for side effects and forward the value untransformed.
    ///
    /// Returns a [`TapPipeline`] node. Materialize to observe.
    #[track_caller]
    fn tap<F>(self, f: F) -> TapPipeline<Self, T, F>
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

impl<T: CellValue, S: Seedness, P: Pipeline<T, S>> TapExt<T, S> for P {}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    };

    use super::*;
    use crate::{Cell, Gettable, MaterializeDefinite, Mutable};

    #[test]
    fn test_tap_side_effect() {
        let source = Cell::new(0u64);
        let side_effect = Arc::new(AtomicU64::new(0));

        let se = side_effect.clone();
        let tapped = source.clone().tap(move |v| {
            se.store(*v, Ordering::SeqCst);
        }).materialize();

        source.set(42);
        assert_eq!(side_effect.load(Ordering::SeqCst), 42);
        assert_eq!(tapped.get(), 42); // value passes through unchanged
    }
}
