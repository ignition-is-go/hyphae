use std::{
    any::Any,
    marker::PhantomData,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

use parking_lot::Mutex;
use uuid::Uuid;

use super::{CellValue, Watchable};
use crate::{
    cell::{Cell, CellMutable},
    pipeline::{Definite, Pipeline, PipelineInstall, PipelineSeed},
    signal::Signal,
    subscription::SubscriptionGuard,
};

pub struct RetryPipeline<S, T, P> {
    source: Arc<S>,
    policy: Arc<P>,
    _type: PhantomData<fn() -> T>,
}

fn install_attempt<S, T, P>(
    source: Arc<S>,
    output: Cell<T, CellMutable>,
    attempts: Arc<AtomicUsize>,
    policy: Arc<P>,
    key: Uuid,
    generation: Arc<Mutex<u64>>,
    skip_seed: bool,
) where
    S: PipelineInstall<T>,
    T: CellValue,
    P: Fn(&anyhow::Error, usize) -> bool + Send + Sync + 'static,
{
    let my_generation = {
        let mut generation = generation.lock();
        *generation = generation.wrapping_add(1);
        *generation
    };
    let first = AtomicBool::new(skip_seed);
    let weak = output.downgrade();
    let generation_for_callback = generation.clone();
    let guard = source.clone().install(Arc::new(move |signal| {
        let Some(output) = weak.upgrade() else { return };
        match signal {
            Signal::Value(_) if first.swap(false, Ordering::SeqCst) => {}
            Signal::Value(_) => {
                attempts.store(0, Ordering::SeqCst);
                output.notify(signal.clone());
            }
            Signal::Complete => output.notify(Signal::Complete),
            Signal::Error(error) => {
                let attempt = attempts.fetch_add(1, Ordering::SeqCst) + 1;
                if policy(error.as_ref(), attempt) {
                    install_attempt(
                        source.clone(),
                        output.clone(),
                        attempts.clone(),
                        policy.clone(),
                        key,
                        generation_for_callback.clone(),
                        false,
                    );
                } else {
                    output.notify(Signal::Error(error.clone()));
                }
            }
        }
    }));

    // A synchronous error may already have installed a later attempt while
    // this `install` call was on the stack. Compare and own under the same lock
    // used to allocate generations, so an older guard can never overwrite it.
    let generation = generation.lock();
    if *generation == my_generation {
        output.own_keyed(key, guard);
    } else {
        drop(guard);
    }
}

impl<S, T, P> PipelineInstall<T> for RetryPipeline<S, T, P>
where
    S: PipelineInstall<T> + PipelineSeed<T>,
    T: CellValue,
    P: Fn(&anyhow::Error, usize) -> bool + Send + Sync + 'static,
{
    fn install(&self, callback: Arc<dyn Fn(&Signal<T>) + Send + Sync>) -> SubscriptionGuard {
        let output = Cell::<T, CellMutable>::new(self.source.seed());
        let key = Uuid::new_v4();
        install_attempt(
            self.source.clone(),
            output.clone(),
            Arc::new(AtomicUsize::new(0)),
            self.policy.clone(),
            key,
            Arc::new(Mutex::new(0)),
            true,
        );
        output.subscribe(move |signal| callback(signal))
    }
}
impl<S, T, P> PipelineSeed<T> for RetryPipeline<S, T, P>
where
    S: PipelineSeed<T>,
    T: CellValue,
    P: Fn(&anyhow::Error, usize) -> bool + Send + Sync + 'static,
{
    fn seed(&self) -> T {
        self.source.seed()
    }
}
impl<S, T, P> Pipeline<T, Definite> for RetryPipeline<S, T, P>
where
    S: Pipeline<T, Definite> + PipelineSeed<T>,
    T: CellValue,
    P: Fn(&anyhow::Error, usize) -> bool + Send + Sync + 'static,
{
}

pub trait RetryExt<T: CellValue>: Pipeline<T, Definite> + PipelineSeed<T> {
    fn retry(self, max_attempts: usize) -> impl crate::Materialize<T, Definite> {
        RetryPipeline {
            source: Arc::new(self),
            policy: Arc::new(move |_: &anyhow::Error, attempt| attempt < max_attempts),
            _type: PhantomData,
        }
    }
    fn retry_when<F>(self, predicate: F) -> impl crate::Materialize<T, Definite>
    where
        F: Fn(&dyn Any, usize) -> bool + Send + Sync + 'static,
    {
        RetryPipeline {
            source: Arc::new(self),
            policy: Arc::new(move |error: &anyhow::Error, attempt| {
                predicate(error as &dyn Any, attempt)
            }),
            _type: PhantomData,
        }
    }
}
impl<T: CellValue, P> RetryExt<T> for P where P: Pipeline<T, Definite> + PipelineSeed<T> {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Cell, DepNode, Materialize, Mutable};

    #[test]
    fn retry_is_lazy_until_materialized() {
        let source = Cell::new(0);
        let pipeline = source.clone().retry(3);
        assert_eq!(source.subscriber_count(), 0);
        let _retried = pipeline.materialize();
        assert_eq!(source.subscriber_count(), 1);
    }
    use std::sync::atomic::AtomicU32;
    #[test]
    fn test_retry_passes_values() {
        let source = Cell::new(0);
        let retried = source.clone().retry(3).materialize();
        let count = Arc::new(AtomicU32::new(0));
        let c = count.clone();
        let _guard = retried.subscribe(move |signal| {
            if signal.is_value() {
                c.fetch_add(1, Ordering::SeqCst);
            }
        });
        source.set(1);
        source.set(2);
        assert_eq!(count.load(Ordering::SeqCst), 3);
    }
    #[test]
    fn test_retry_retries_on_error() {
        let source = Cell::new(0);
        let retried = source.clone().retry(1).materialize();
        let errors = Arc::new(AtomicU32::new(0));
        let count = errors.clone();
        let _guard = retried.subscribe(move |signal| {
            if signal.is_error() {
                count.fetch_add(1, Ordering::SeqCst);
            }
        });
        source.fail(anyhow::anyhow!("error"));
        assert_eq!(errors.load(Ordering::SeqCst), 1);
    }
}
