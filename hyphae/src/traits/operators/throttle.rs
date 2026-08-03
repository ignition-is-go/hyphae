use std::{
    marker::PhantomData,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use super::CellValue;
use crate::{
    pipeline::{Definite, MaterializeDefinite, Pipeline, PipelineInstall, PipelineSeed},
    platform,
    signal::Signal,
    subscription::SubscriptionGuard,
};

pub struct ThrottlePipeline<S, T> {
    source: S,
    duration: Duration,
    _t: PhantomData<fn(T)>,
}

impl<S: PipelineInstall<T>, T: CellValue> PipelineInstall<T> for ThrottlePipeline<S, T> {
    fn install(&self, callback: Arc<dyn Fn(&Signal<T>) + Send + Sync>) -> SubscriptionGuard {
        let can_emit = Arc::new(AtomicBool::new(true));
        let duration = self.duration;
        self.source.install(Arc::new(move |signal| match signal {
            Signal::Value(_) => {
                if can_emit.swap(false, Ordering::SeqCst) {
                    callback(signal);
                    let can_emit = can_emit.clone();
                    platform::spawn_delayed(duration, move || {
                        can_emit.store(true, Ordering::SeqCst);
                    });
                }
            }
            Signal::Complete => callback(&Signal::Complete),
            Signal::Error(error) => callback(&Signal::Error(error.clone())),
        }))
    }
}

impl<S: PipelineSeed<T>, T: CellValue> PipelineSeed<T> for ThrottlePipeline<S, T> {
    fn seed(&self) -> T {
        self.source.seed()
    }
}

#[allow(private_bounds)]
impl<S: Pipeline<T, Definite> + PipelineSeed<T>, T: CellValue> Pipeline<T, Definite>
    for ThrottlePipeline<S, T>
{
}

impl<S: Pipeline<T, Definite> + PipelineSeed<T>, T: CellValue> MaterializeDefinite<T>
    for ThrottlePipeline<S, T>
{
}

#[allow(private_bounds)]
pub trait ThrottleExt<T: CellValue>: Pipeline<T, Definite> + PipelineSeed<T> {
    #[track_caller]
    fn throttle(self, duration: Duration) -> ThrottlePipeline<Self, T> {
        ThrottlePipeline {
            source: self,
            duration,
            _t: PhantomData,
        }
    }
}

impl<T: CellValue, P: Pipeline<T, Definite> + PipelineSeed<T>> ThrottleExt<T> for P {}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicU64;

    use super::*;
    use crate::{Cell, MaterializeDefinite, Mutable, traits::Watchable};

    #[test]
    fn test_throttle_limits_rate() {
        let source = Cell::new(0u64);
        let throttled = source
            .clone()
            .throttle(Duration::from_millis(50))
            .materialize();
        let count = Arc::new(AtomicU64::new(0));

        let c = count.clone();
        let _guard = throttled.subscribe(move |_| {
            c.fetch_add(1, Ordering::SeqCst);
        });

        for i in 1..=10 {
            source.set(i);
        }
        assert!(count.load(Ordering::SeqCst) < 10);
    }
}
