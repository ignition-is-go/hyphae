use std::{
    marker::PhantomData,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
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

pub struct DebouncePipeline<S, T> {
    source: S,
    duration: Duration,
    _t: PhantomData<fn(T)>,
}

impl<S: PipelineInstall<T>, T: CellValue> PipelineInstall<T> for DebouncePipeline<S, T> {
    fn install(&self, callback: Arc<dyn Fn(&Signal<T>) + Send + Sync>) -> SubscriptionGuard {
        let generation = Arc::new(AtomicU64::new(0));
        let duration = self.duration;
        self.source.install(Arc::new(move |signal| match signal {
            Signal::Value(value) => {
                let my_generation = generation.fetch_add(1, Ordering::SeqCst) + 1;
                let generation = generation.clone();
                let callback = callback.clone();
                let value = value.clone();
                platform::spawn_delayed(duration, move || {
                    if generation.load(Ordering::SeqCst) == my_generation {
                        callback(&Signal::value_arc(value));
                    }
                });
            }
            Signal::Complete => callback(&Signal::Complete),
            Signal::Error(error) => callback(&Signal::Error(error.clone())),
        }))
    }
}

impl<S: PipelineSeed<T>, T: CellValue> PipelineSeed<T> for DebouncePipeline<S, T> {
    fn seed(&self) -> T {
        self.source.seed()
    }
}

#[allow(private_bounds)]
impl<S: Pipeline<T, Definite> + PipelineSeed<T>, T: CellValue> Pipeline<T, Definite>
    for DebouncePipeline<S, T>
{
}

impl<S: Pipeline<T, Definite> + PipelineSeed<T>, T: CellValue> MaterializeDefinite<T>
    for DebouncePipeline<S, T>
{
}

#[allow(private_bounds)]
pub trait DebounceExt<T: CellValue>: Pipeline<T, Definite> + PipelineSeed<T> {
    #[track_caller]
    fn debounce(self, duration: Duration) -> DebouncePipeline<Self, T> {
        DebouncePipeline {
            source: self,
            duration,
            _t: PhantomData,
        }
    }
}

impl<T: CellValue, P: Pipeline<T, Definite> + PipelineSeed<T>> DebounceExt<T> for P {}

#[cfg(test)]
mod tests {
    use std::{sync::atomic::AtomicU64, thread};

    use super::*;
    use crate::{Cell, MaterializeDefinite, Mutable, traits::Watchable};

    #[test]
    fn test_debounce_waits_for_pause() {
        let source = Cell::new(0u64);
        let debounced = source
            .clone()
            .debounce(Duration::from_millis(50))
            .materialize();
        let received = Arc::new(AtomicU64::new(0));

        let r = received.clone();
        let _guard = debounced.subscribe(move |signal| {
            if let Signal::Value(v) = signal {
                r.store(**v, Ordering::SeqCst);
            }
        });

        source.set(1);
        source.set(2);
        source.set(3);
        thread::sleep(Duration::from_millis(10));
        assert_eq!(received.load(Ordering::SeqCst), 0);
        thread::sleep(Duration::from_millis(100));
        assert_eq!(received.load(Ordering::SeqCst), 3);
    }
}
