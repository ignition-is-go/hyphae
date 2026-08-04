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

pub struct TimeoutPipeline<S, T> {
    source: S,
    duration: Duration,
    _t: PhantomData<fn(T)>,
}

impl<S: PipelineInstall<T>, T: CellValue> PipelineInstall<T> for TimeoutPipeline<S, T> {
    fn install(&self, callback: Arc<dyn Fn(&Signal<T>) + Send + Sync>) -> SubscriptionGuard {
        let generation = Arc::new(AtomicU64::new(0));
        let completed = Arc::new(AtomicBool::new(false));
        let first = AtomicBool::new(true);
        let duration = self.duration;

        let initial_generation = generation.load(Ordering::SeqCst);
        let initial_generation_ref = generation.clone();
        let initial_completed = completed.clone();
        let initial_callback = callback.clone();
        platform::spawn_delayed(duration, move || {
            if !initial_completed.load(Ordering::SeqCst)
                && initial_generation_ref.load(Ordering::SeqCst) == initial_generation
                && initial_completed
                    .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
            {
                initial_callback(&Signal::error(anyhow::anyhow!("timeout")));
            }
        });

        self.source.install(Arc::new(move |signal| match signal {
            Signal::Value(value) => {
                if completed.load(Ordering::SeqCst) {
                    return;
                }
                if first.swap(false, Ordering::SeqCst) {
                    return;
                }
                let new_generation = generation.fetch_add(1, Ordering::SeqCst) + 1;
                callback(&Signal::Value(value.clone()));
                let generation = generation.clone();
                let completed = completed.clone();
                let callback = callback.clone();
                platform::spawn_delayed(duration, move || {
                    if !completed.load(Ordering::SeqCst)
                        && generation.load(Ordering::SeqCst) == new_generation
                        && completed
                            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                            .is_ok()
                    {
                        callback(&Signal::error(anyhow::anyhow!("timeout")));
                    }
                });
            }
            Signal::Complete => {
                completed.store(true, Ordering::SeqCst);
                callback(&Signal::Complete);
            }
            Signal::Error(error) => {
                completed.store(true, Ordering::SeqCst);
                callback(&Signal::Error(error.clone()));
            }
        }))
    }
}

impl<S: PipelineSeed<T>, T: CellValue> PipelineSeed<T> for TimeoutPipeline<S, T> {
    fn seed(&self) -> T {
        self.source.seed()
    }
}

#[allow(private_bounds)]
impl<S: Pipeline<T, Definite> + PipelineSeed<T>, T: CellValue> Pipeline<T, Definite>
    for TimeoutPipeline<S, T>
{
}

#[allow(private_bounds)]
pub trait TimeoutExt<T: CellValue>: Pipeline<T, Definite> + PipelineSeed<T> {
    #[track_caller]
    fn timeout(self, duration: Duration) -> impl crate::Materialize<T, Definite> {
        TimeoutPipeline {
            source: self,
            duration,
            _t: PhantomData,
        }
    }
}

impl<T: CellValue, P: Pipeline<T, Definite> + PipelineSeed<T>> TimeoutExt<T> for P {}

#[cfg(test)]
mod tests {
    use std::{sync::atomic::AtomicU32, thread};

    use super::*;
    use crate::{Cell, Materialize, Mutable, traits::Watchable};

    fn error_count<T: CellValue>(
        timed: &crate::Cell<T, crate::CellImmutable>,
    ) -> (Arc<AtomicU32>, SubscriptionGuard) {
        let count = Arc::new(AtomicU32::new(0));
        let copy = count.clone();
        let guard = timed.subscribe(move |signal| {
            if let Signal::Error(_) = signal {
                copy.fetch_add(1, Ordering::SeqCst);
            }
        });
        (count, guard)
    }

    #[test]
    fn test_timeout_no_timeout_when_active() {
        let source = Cell::new(0);
        let timed = source
            .clone()
            .timeout(Duration::from_millis(50))
            .materialize();
        let (count, _guard) = error_count(&timed);
        for i in 1..=5 {
            thread::sleep(Duration::from_millis(20));
            source.set(i);
        }
        thread::sleep(Duration::from_millis(10));
        assert_eq!(count.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn test_timeout_triggers_on_inactivity() {
        let source = Cell::new(0);
        let timed = source.timeout(Duration::from_millis(30)).materialize();
        let (count, _guard) = error_count(&timed);
        thread::sleep(Duration::from_millis(50));
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn test_timeout_no_error_after_complete() {
        let source = Cell::new(0);
        let timed = source
            .clone()
            .timeout(Duration::from_millis(30))
            .materialize();
        let (count, _guard) = error_count(&timed);
        source.complete();
        thread::sleep(Duration::from_millis(50));
        assert_eq!(count.load(Ordering::SeqCst), 0);
    }
}
