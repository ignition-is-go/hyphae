use parking_lot::Mutex;
use std::{
    marker::PhantomData,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use super::CellValue;
use crate::{
    pipeline::{Definite, Pipeline, PipelineInstall, PipelineSeed},
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
        let terminated = Arc::new(AtomicBool::new(false));
        let pending = Arc::new(Mutex::new(None::<Arc<T>>));
        let first = AtomicBool::new(true);
        let duration = self.duration;
        self.source.install(Arc::new(move |signal| match signal {
            Signal::Value(value) => {
                if first.swap(false, Ordering::SeqCst) || terminated.load(Ordering::SeqCst) {
                    return;
                }
                let my_generation = generation.fetch_add(1, Ordering::SeqCst).saturating_add(1);
                let generation = generation.clone();
                let terminated = terminated.clone();
                let pending = pending.clone();
                let callback = callback.clone();
                *pending.lock() = Some(value.clone());
                platform::spawn_delayed(duration, move || {
                    if !terminated.load(Ordering::SeqCst)
                        && generation.load(Ordering::SeqCst) == my_generation
                        && let Some(value) = pending.lock().take()
                    {
                        callback(&Signal::value_arc(value));
                    }
                });
            }
            Signal::Complete => {
                if !terminated.swap(true, Ordering::SeqCst) {
                    generation.fetch_add(1, Ordering::SeqCst);
                    let value = pending.lock().take();
                    if let Some(value) = value {
                        callback(&Signal::value_arc(value));
                    }
                    callback(&Signal::Complete);
                }
            }
            Signal::Error(error) => {
                if !terminated.swap(true, Ordering::SeqCst) {
                    generation.fetch_add(1, Ordering::SeqCst);
                    pending.lock().take();
                    callback(&Signal::Error(error.clone()));
                }
            }
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

#[allow(private_bounds)]
pub trait DebounceExt<T: CellValue>: Pipeline<T, Definite> + PipelineSeed<T> {
    #[track_caller]
    fn debounce(self, duration: Duration) -> impl crate::Materialize<T, Definite> {
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
    use crate::{Cell, Materialize, Mutable, traits::Watchable};

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

    #[test]
    fn debounce_flushes_before_complete_and_never_emits_after_terminal() {
        let source = Cell::new(0u64);
        let events = Arc::new(Mutex::new(Vec::new()));
        let captured = events.clone();
        let pipeline = source.clone().debounce(Duration::from_millis(20));
        let _guard = pipeline.install(Arc::new(move |signal| {
            captured.lock().push(match signal {
                Signal::Value(value) => format!("value:{}", **value),
                Signal::Complete => "complete".into(),
                Signal::Error(_) => "error".into(),
            });
        }));

        source.set(7);
        source.complete();
        thread::sleep(Duration::from_millis(60));

        assert_eq!(&*events.lock(), &["value:7", "complete"]);
    }
}
