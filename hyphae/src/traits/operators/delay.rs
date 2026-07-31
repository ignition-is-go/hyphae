use std::{marker::PhantomData, sync::Arc, time::Duration};

use super::CellValue;
use crate::{
    pipeline::{Definite, MaterializeDefinite, Pipeline, PipelineInstall, PipelineSeed},
    platform,
    signal::Signal,
    subscription::SubscriptionGuard,
};

pub struct DelayPipeline<S, T> {
    source: S,
    duration: Duration,
    _t: PhantomData<fn(T)>,
}

impl<S: PipelineInstall<T>, T: CellValue> PipelineInstall<T> for DelayPipeline<S, T> {
    fn install(&self, callback: Arc<dyn Fn(&Signal<T>) + Send + Sync>) -> SubscriptionGuard {
        let duration = self.duration;
        self.source.install(Arc::new(move |signal| {
            let signal = signal.clone();
            let callback = callback.clone();
            platform::spawn_delayed(duration, move || callback(&signal));
        }))
    }
}

impl<S: PipelineSeed<T>, T: CellValue> PipelineSeed<T> for DelayPipeline<S, T> {
    fn seed(&self) -> T {
        self.source.seed()
    }
}

#[allow(private_bounds)]
impl<S: Pipeline<T, Definite> + PipelineSeed<T>, T: CellValue> Pipeline<T, Definite>
    for DelayPipeline<S, T>
{
}

impl<S: Pipeline<T, Definite> + PipelineSeed<T>, T: CellValue> MaterializeDefinite<T>
    for DelayPipeline<S, T>
{
}

#[allow(private_bounds)]
pub trait DelayExt<T: CellValue>: Pipeline<T, Definite> + PipelineSeed<T> {
    #[track_caller]
    fn delay(self, duration: Duration) -> DelayPipeline<Self, T> {
        DelayPipeline {
            source: self,
            duration,
            _t: PhantomData,
        }
    }
}

impl<T: CellValue, P: Pipeline<T, Definite> + PipelineSeed<T>> DelayExt<T> for P {}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicU64, Ordering},
        },
        thread,
    };

    use super::*;
    use crate::{Cell, MaterializeDefinite, Mutable, Signal, traits::Watchable};

    #[test]
    fn test_delay_delays_emission() {
        let source = Cell::new(0u64);
        let delayed = source
            .clone()
            .delay(Duration::from_millis(50))
            .materialize();
        let received = Arc::new(AtomicU64::new(0));

        let r = received.clone();
        let _guard = delayed.subscribe(move |signal| {
            if let Signal::Value(v) = signal {
                r.store(**v, Ordering::SeqCst);
            }
        });

        thread::sleep(Duration::from_millis(100));
        assert_eq!(received.load(Ordering::SeqCst), 0);
        source.set(42);
        thread::sleep(Duration::from_millis(20));
        assert_eq!(received.load(Ordering::SeqCst), 0);
        thread::sleep(Duration::from_millis(100));
        assert_eq!(received.load(Ordering::SeqCst), 42);
    }
}
