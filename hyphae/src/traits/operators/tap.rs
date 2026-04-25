//! `tap` operator — pure side effect; chains fuse into one closure on root.

use std::{marker::PhantomData, sync::Arc};

use super::CellValue;
use crate::{
    pipeline::{Pipeline, PipelineInstall},
    signal::Signal,
    subscription::SubscriptionGuard,
    traits::Gettable,
};

/// Pipeline node representing `source.tap(f)`. Does not allocate a cell.
pub struct TapPipeline<S, T, F> {
    source: S,
    f: Arc<F>,
    _t: PhantomData<fn(T)>,
}

impl<S: Clone, T, F> Clone for TapPipeline<S, T, F> {
    fn clone(&self) -> Self {
        Self {
            source: self.source.clone(),
            f: Arc::clone(&self.f),
            _t: PhantomData,
        }
    }
}

impl<S, T, F> Gettable<T> for TapPipeline<S, T, F>
where
    S: Gettable<T>,
    F: Fn(&T),
{
    fn get(&self) -> T {
        let v = self.source.get();
        (self.f)(&v);
        v
    }
}

impl<S, T, F> PipelineInstall<T> for TapPipeline<S, T, F>
where
    S: PipelineInstall<T> + Clone + Send + Sync + 'static,
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

#[allow(private_bounds)]
impl<S, T, F> Pipeline<T> for TapPipeline<S, T, F>
where
    S: Pipeline<T>,
    T: CellValue,
    F: Fn(&T) + Send + Sync + 'static,
{
}

/// Extension trait for side-effecting observation.
pub trait TapExt<T: CellValue>: Pipeline<T> {
    /// Run `f(&value)` for side effects and forward the value untransformed.
    ///
    /// Returns a [`TapPipeline`] node. Materialize to observe.
    #[track_caller]
    fn tap<F>(&self, f: F) -> TapPipeline<Self, T, F>
    where
        F: Fn(&T) + Send + Sync + 'static,
    {
        TapPipeline {
            source: self.clone(),
            f: Arc::new(f),
            _t: PhantomData,
        }
    }
}

impl<T: CellValue, P: Pipeline<T>> TapExt<T> for P {}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    };

    use super::*;
    use crate::{Cell, Mutable};

    #[test]
    fn test_tap_side_effect() {
        let source = Cell::new(0u64);
        let side_effect = Arc::new(AtomicU64::new(0));

        let se = side_effect.clone();
        let tapped = source.tap(move |v| {
            se.store(*v, Ordering::SeqCst);
        }).materialize();

        source.set(42);
        assert_eq!(side_effect.load(Ordering::SeqCst), 42);
        assert_eq!(tapped.get(), 42); // value passes through unchanged
    }
}
