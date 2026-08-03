use std::{
    marker::PhantomData,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
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

pub struct AuditPipeline<S, T> {
    source: S,
    duration: Duration,
    _t: PhantomData<fn(T)>,
}

impl<S: PipelineInstall<T>, T: CellValue> PipelineInstall<T> for AuditPipeline<S, T> {
    fn install(&self, callback: Arc<dyn Fn(&Signal<T>) + Send + Sync>) -> SubscriptionGuard {
        let first = AtomicBool::new(true);
        let latest: Arc<Mutex<Option<T>>> = Arc::new(Mutex::new(None));
        let generation = Arc::new(AtomicU64::new(0));
        let in_window = Arc::new(AtomicBool::new(false));
        let duration = self.duration;

        self.source.install(Arc::new(move |signal| match signal {
            Signal::Value(value) => {
                if first.swap(false, Ordering::SeqCst) {
                    return;
                }
                *latest.lock().expect("audit poisoned") = Some(value.as_ref().clone());
                if !in_window.swap(true, Ordering::SeqCst) {
                    let current_generation = generation.fetch_add(1, Ordering::SeqCst) + 1;
                    let latest = latest.clone();
                    let generation = generation.clone();
                    let in_window = in_window.clone();
                    let callback = callback.clone();
                    platform::spawn_delayed(duration, move || {
                        if generation.load(Ordering::SeqCst) == current_generation {
                            if let Some(value) = latest.lock().expect("audit poisoned").clone() {
                                callback(&Signal::value(value));
                            }
                            in_window.store(false, Ordering::SeqCst);
                        }
                    });
                }
            }
            Signal::Complete => callback(&Signal::Complete),
            Signal::Error(error) => callback(&Signal::Error(error.clone())),
        }))
    }
}

impl<S: PipelineSeed<T>, T: CellValue> PipelineSeed<T> for AuditPipeline<S, T> {
    fn seed(&self) -> T {
        self.source.seed()
    }
}

#[allow(private_bounds)]
impl<S: Pipeline<T, Definite> + PipelineSeed<T>, T: CellValue> Pipeline<T, Definite>
    for AuditPipeline<S, T>
{
}

impl<S: Pipeline<T, Definite> + PipelineSeed<T>, T: CellValue> MaterializeDefinite<T>
    for AuditPipeline<S, T>
{
}

#[allow(private_bounds)]
pub trait AuditExt<T: CellValue>: Pipeline<T, Definite> + PipelineSeed<T> {
    #[track_caller]
    fn audit(self, duration: Duration) -> AuditPipeline<Self, T> {
        AuditPipeline {
            source: self,
            duration,
            _t: PhantomData,
        }
    }
}

impl<T: CellValue, P: Pipeline<T, Definite> + PipelineSeed<T>> AuditExt<T> for P {}

#[cfg(test)]
mod tests {
    use std::{sync::atomic::AtomicU32, thread};

    use super::*;
    use crate::{Cell, Gettable, MaterializeDefinite, Mutable, traits::Watchable};

    #[test]
    fn test_audit_emits_last() {
        let source = Cell::new(0);
        let audited = source
            .clone()
            .audit(Duration::from_millis(50))
            .materialize();
        let emissions = Arc::new(AtomicU32::new(0));
        let e = emissions.clone();
        let _guard = audited.subscribe(move |signal| {
            if let Signal::Value(_) = signal {
                e.fetch_add(1, Ordering::SeqCst);
            }
        });

        assert_eq!(emissions.load(Ordering::SeqCst), 1);
        source.set(1);
        source.set(2);
        source.set(3);
        assert_eq!(emissions.load(Ordering::SeqCst), 1);
        thread::sleep(Duration::from_millis(70));
        assert_eq!(emissions.load(Ordering::SeqCst), 2);
        assert_eq!(audited.get(), 3);
    }
}
