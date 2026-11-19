use std::{
    marker::PhantomData,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use super::{
    CellValue,
    fan_in::{Arrival, fan_in},
};
use crate::{
    pipeline::{Definite, Pipeline, PipelineInstall, PipelineSeed},
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
        let stopped = AtomicBool::new(false);
        let notifier_first = AtomicBool::new(true);
        let source_first = AtomicBool::new(true);
        fan_in(&self.notifier, &self.source, move |arrival| match arrival {
            Arrival::Left(Signal::Value(_)) => {
                if !notifier_first.swap(false, Ordering::SeqCst) {
                    stopped.store(true, Ordering::SeqCst);
                    callback(&Signal::Complete);
                }
            }
            Arrival::Left(_) => {}
            Arrival::Right(Signal::Value(_)) if source_first.swap(false, Ordering::SeqCst) => {}
            Arrival::Right(Signal::Value(_)) if stopped.load(Ordering::SeqCst) => {}
            Arrival::Right(signal) => callback(&signal),
        })
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

pub trait TakeUntilExt<T: CellValue>: Pipeline<T, Definite> + PipelineSeed<T> {
    fn take_until<U, N>(self, notifier: N) -> impl crate::Materialize<T, Definite>
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
    use crate::{Cell, Gettable, Materialize, Mutable, Watchable};
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
