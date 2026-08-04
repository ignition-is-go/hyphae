use std::{
    marker::PhantomData,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use parking_lot::Mutex;

use super::{CellValue, Watchable};
use crate::{
    cell::{Cell, CellMutable},
    pipeline::{Definite, MaterializeDefinite, Pipeline, PipelineInstall, PipelineSeed},
    signal::Signal,
    subscription::SubscriptionGuard,
};
use uuid::Uuid;

pub struct ConcatPipeline<A, B, T> {
    first: A,
    second: Arc<B>,
    _type: PhantomData<fn() -> T>,
}

impl<A, B, T> PipelineInstall<T> for ConcatPipeline<A, B, T>
where
    A: PipelineInstall<T> + PipelineSeed<T>,
    B: PipelineInstall<T>,
    T: CellValue,
{
    fn install(&self, callback: Arc<dyn Fn(&Signal<T>) + Send + Sync>) -> SubscriptionGuard {
        // The output cell is the dynamic subscription slot: on completion it
        // atomically takes ownership of the second root and updates scheduler
        // dependency height through `Cell::own_keyed`.
        let output = Cell::<T, CellMutable>::new(self.first.seed());
        let root_key = Uuid::new_v4();
        let weak = output.downgrade();
        let first_skip = AtomicBool::new(true);
        let second = self.second.clone();
        // Installing a pipeline may synchronously replay Complete. Serialize
        // the completion switch with ownership of the returned first guard so
        // the latter can never overwrite an already-installed second guard.
        let switched = Arc::new(Mutex::new(false));
        let switched_on_complete = switched.clone();
        let first_guard = self.first.install(Arc::new(move |signal| {
            if let Some(output) = weak.upgrade() {
                match signal {
                    Signal::Value(_) if first_skip.swap(false, Ordering::SeqCst) => {}
                    Signal::Value(_) => output.notify(signal.clone()),
                    Signal::Complete => {
                        let mut switched = switched_on_complete.lock();
                        if *switched {
                            return;
                        }
                        *switched = true;
                        let weak_second = output.downgrade();
                        let second_skip = AtomicBool::new(true);
                        let guard = second.install(Arc::new(move |signal| {
                            if let Some(output) = weak_second.upgrade() {
                                match signal {
                                    Signal::Value(_)
                                        if second_skip.swap(false, Ordering::SeqCst) => {}
                                    _ => output.notify(signal.clone()),
                                }
                            }
                        }));
                        output.own_keyed(root_key, guard);
                    }
                    Signal::Error(_) => output.notify(signal.clone()),
                }
            }
        }));
        let switched = switched.lock();
        if *switched {
            drop(first_guard);
        } else {
            output.own_keyed(root_key, first_guard);
        }
        output.subscribe(move |signal| callback(signal))
    }
}

impl<A, B, T> PipelineSeed<T> for ConcatPipeline<A, B, T>
where
    A: PipelineSeed<T>,
    B: PipelineInstall<T>,
    T: CellValue,
{
    fn seed(&self) -> T {
        self.first.seed()
    }
}
impl<A, B, T> Pipeline<T, Definite> for ConcatPipeline<A, B, T>
where
    A: Pipeline<T, Definite> + PipelineSeed<T>,
    B: Pipeline<T, Definite>,
    T: CellValue,
{
}
impl<A, B, T> MaterializeDefinite<T> for ConcatPipeline<A, B, T>
where
    A: Pipeline<T, Definite> + PipelineSeed<T>,
    B: Pipeline<T, Definite>,
    T: CellValue,
{
}

pub trait ConcatExt<T: CellValue>: Pipeline<T, Definite> + PipelineSeed<T> {
    fn concat<B>(self, second: B) -> ConcatPipeline<Self, B, T>
    where
        B: Pipeline<T, Definite>,
    {
        ConcatPipeline {
            first: self,
            second: Arc::new(second),
            _type: PhantomData,
        }
    }
}
impl<T: CellValue, P> ConcatExt<T> for P where P: Pipeline<T, Definite> + PipelineSeed<T> {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Cell, DepNode, Gettable, MaterializeDefinite, Mutable};

    #[test]
    fn concat_is_lazy_until_materialized() {
        let first = Cell::new(1);
        let second = Cell::new(2);
        let pipeline = first.clone().concat(second.clone());
        assert_eq!(first.subscriber_count(), 0);
        assert_eq!(second.subscriber_count(), 0);
        let _combined = pipeline.materialize();
        assert_eq!(first.subscriber_count(), 1);
        assert_eq!(second.subscriber_count(), 0);
        first.complete();
        assert_eq!(first.subscriber_count(), 0);
        assert_eq!(second.subscriber_count(), 1);
    }
    #[test]
    fn test_concat() {
        let first = Cell::new(1);
        let second = Cell::new(100);
        let combined = first.clone().concat(second.clone()).materialize();
        first.set(2);
        assert_eq!(combined.get(), 2);
        first.complete();
        second.set(200);
        assert_eq!(combined.get(), 200);
    }

    #[test]
    fn concat_preserves_second_subscription_when_first_is_already_complete() {
        let first = Cell::new(1);
        let second = Cell::new(100);
        first.complete();

        let combined = first.clone().concat(second.clone()).materialize();
        assert_eq!(first.subscriber_count(), 0);
        assert_eq!(second.subscriber_count(), 1);

        second.set(200);
        assert_eq!(combined.get(), 200);
    }
}
