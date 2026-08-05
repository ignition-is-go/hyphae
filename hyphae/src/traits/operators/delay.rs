use parking_lot::Mutex;
use std::{
    collections::BTreeMap,
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

pub struct DelayPipeline<S, T> {
    source: S,
    duration: Duration,
    _t: PhantomData<fn(T)>,
}

impl<S: PipelineInstall<T>, T: CellValue> PipelineInstall<T> for DelayPipeline<S, T> {
    fn install(&self, callback: Arc<dyn Fn(&Signal<T>) + Send + Sync>) -> SubscriptionGuard {
        let duration = self.duration;
        let first = AtomicBool::new(true);
        let sequence = Arc::new(AtomicU64::new(0));
        let ready = Arc::new(Mutex::new((0_u64, BTreeMap::<u64, Signal<T>>::new())));
        self.source.install(Arc::new(move |signal| {
            if matches!(signal, Signal::Value(_)) && first.swap(false, Ordering::SeqCst) {
                return;
            }
            let my_sequence = sequence.fetch_add(1, Ordering::SeqCst);
            let signal = signal.clone();
            let callback = callback.clone();
            let ready = ready.clone();
            platform::spawn_delayed(duration, move || {
                let deliver = {
                    let mut state = ready.lock();
                    state.1.insert(my_sequence, signal);
                    let mut deliver = Vec::new();
                    loop {
                        let next = state.0;
                        let Some(signal) = state.1.remove(&next) else {
                            break;
                        };
                        state.0 = state.0.saturating_add(1);
                        deliver.push(signal);
                    }
                    drop(state);
                    deliver
                };
                for signal in deliver {
                    callback(&signal);
                }
            });
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

#[allow(private_bounds)]
pub trait DelayExt<T: CellValue>: Pipeline<T, Definite> + PipelineSeed<T> {
    #[track_caller]
    fn delay(self, duration: Duration) -> impl crate::Materialize<T, Definite> {
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
    use crate::{Cell, Materialize, Mutable, Signal, traits::Watchable};

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

    #[test]
    fn delay_preserves_value_before_terminal_order() {
        let source = Cell::new(0u64);
        let events = Arc::new(Mutex::new(Vec::new()));
        let captured = events.clone();
        let pipeline = source.clone().delay(Duration::from_millis(20));
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
