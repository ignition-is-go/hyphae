use std::{
    marker::PhantomData,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use super::CellValue;
use crate::{
    pipeline::{Definite, MaterializeDefinite, Pipeline, PipelineInstall, PipelineSeed},
    signal::Signal,
    subscription::SubscriptionGuard,
};

pub struct TakeUntilPipeline<S, N, T, U> {
    source: S,
    notifier: N,
    _types: PhantomData<fn() -> (T, U)>,
}

impl<S, N, T, U> PipelineInstall<T> for TakeUntilPipeline<S, N, T, U>
where
    S: PipelineInstall<T>,
    N: PipelineInstall<U>,
    T: CellValue,
    U: CellValue,
{
    fn install(&self, callback: Arc<dyn Fn(&Signal<T>) + Send + Sync>) -> SubscriptionGuard {
        let stopped = Arc::new(AtomicBool::new(false));
        let notifier_stopped = stopped.clone();
        let notifier_callback = callback.clone();
        let notifier_first = AtomicBool::new(true);
        let notifier = self.notifier.install(Arc::new(move |signal| {
            if matches!(signal, Signal::Value(_)) && !notifier_first.swap(false, Ordering::SeqCst) {
                notifier_stopped.store(true, Ordering::SeqCst);
                notifier_callback(&Signal::Complete);
            }
        }));
        let source_first = AtomicBool::new(true);
        let source = self.source.install(Arc::new(move |signal| match signal {
            Signal::Value(_) if source_first.swap(false, Ordering::SeqCst) => {}
            Signal::Value(_) if stopped.load(Ordering::SeqCst) => {}
            _ => callback(signal),
        }));
        SubscriptionGuard::combine(vec![notifier, source])
    }
}
impl<S, N, T, U> PipelineSeed<T> for TakeUntilPipeline<S, N, T, U>
where
    S: PipelineSeed<T>,
    N: PipelineInstall<U>,
    T: CellValue,
    U: CellValue,
{
    fn seed(&self) -> T {
        self.source.seed()
    }
}
impl<S, N, T, U> Pipeline<T, Definite> for TakeUntilPipeline<S, N, T, U>
where
    S: Pipeline<T, Definite> + PipelineSeed<T>,
    N: Pipeline<U, Definite>,
    T: CellValue,
    U: CellValue,
{
}
impl<S, N, T, U> MaterializeDefinite<T> for TakeUntilPipeline<S, N, T, U>
where
    S: Pipeline<T, Definite> + PipelineSeed<T>,
    N: Pipeline<U, Definite>,
    T: CellValue,
    U: CellValue,
{
}

pub trait TakeUntilExt<T: CellValue>: Pipeline<T, Definite> + PipelineSeed<T> {
    fn take_until<U, N>(self, notifier: N) -> TakeUntilPipeline<Self, N, T, U>
    where
        U: CellValue,
        N: Pipeline<U, Definite>,
    {
        TakeUntilPipeline {
            source: self,
            notifier,
            _types: PhantomData,
        }
    }
}
impl<T: CellValue, P> TakeUntilExt<T> for P where P: Pipeline<T, Definite> + PipelineSeed<T> {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Cell, Gettable, MaterializeDefinite, Mutable, Watchable};
    #[test]
    fn test_take_until() {
        let source = Cell::new(1u64);
        let stopper = Cell::new(false);
        let taken = source.clone().take_until(stopper.clone()).materialize();
        source.set(2);
        assert_eq!(taken.get(), 2);
        stopper.set(true);
        source.set(3);
        assert_eq!(taken.get(), 2);
    }
    #[test]
    fn test_take_until_completes_on_notifier() {
        let source = Cell::new(1u64);
        let stopper = Cell::new(false);
        let taken = source.take_until(stopper.clone()).materialize();
        stopper.set(true);
        assert!(taken.is_complete());
    }
}
