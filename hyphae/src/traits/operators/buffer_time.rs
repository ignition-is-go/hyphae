use std::{
    marker::PhantomData,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use crossbeam::queue::SegQueue;

use super::CellValue;
use crate::{
    pipeline::{Definite, Pipeline, PipelineInstall, PipelineSeed},
    platform,
    signal::Signal,
    subscription::SubscriptionGuard,
};

pub struct BufferTimePipeline<S, T> {
    source: S,
    duration: Duration,
    _t: PhantomData<fn(T)>,
}

impl<S: PipelineInstall<T>, T: CellValue> PipelineInstall<Vec<T>> for BufferTimePipeline<S, T> {
    fn install(&self, callback: Arc<dyn Fn(&Signal<Vec<T>>) + Send + Sync>) -> SubscriptionGuard {
        let buffer = Arc::new(SegQueue::new());
        let completed = Arc::new(AtomicBool::new(false));
        let first = AtomicBool::new(true);

        let interval_buffer = buffer.clone();
        let interval_completed = completed.clone();
        let interval_callback = callback.clone();
        platform::spawn_interval(self.duration, false, move |_count| {
            if interval_completed.load(Ordering::SeqCst) {
                return false;
            }
            let mut chunk = Vec::new();
            while let Some(value) = interval_buffer.pop() {
                chunk.push(value);
            }
            interval_callback(&Signal::value(chunk));
            true
        });

        let subscription_completed = completed.clone();
        let guard = self.source.install(Arc::new(move |signal| match signal {
            Signal::Value(value) => {
                if first.swap(false, Ordering::SeqCst) {
                    return;
                }
                buffer.push(value.as_ref().clone());
            }
            Signal::Complete => {
                subscription_completed.store(true, Ordering::SeqCst);
                let mut remainder = Vec::new();
                while let Some(value) = buffer.pop() {
                    remainder.push(value);
                }
                if !remainder.is_empty() {
                    callback(&Signal::value(remainder));
                }
                callback(&Signal::Complete);
            }
            Signal::Error(error) => {
                subscription_completed.store(true, Ordering::SeqCst);
                callback(&Signal::Error(error.clone()));
            }
        }));

        guard.with_cleanup(move || completed.store(true, Ordering::SeqCst))
    }
}

impl<S: PipelineInstall<T>, T: CellValue> PipelineSeed<Vec<T>> for BufferTimePipeline<S, T> {
    fn seed(&self) -> Vec<T> {
        Vec::new()
    }
}

#[allow(private_bounds)]
impl<S: Pipeline<T, Definite>, T: CellValue> Pipeline<Vec<T>, Definite>
    for BufferTimePipeline<S, T>
{
}

#[allow(private_bounds)]
pub trait BufferTimeExt<T: CellValue>: Pipeline<T, Definite> {
    #[track_caller]
    fn buffer_time(self, duration: Duration) -> impl crate::Materialize<Vec<T>, Definite> {
        BufferTimePipeline {
            source: self,
            duration,
            _t: PhantomData,
        }
    }
}

impl<T: CellValue, P: Pipeline<T, Definite>> BufferTimeExt<T> for P {}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;
    use crate::{Cell, Materialize, Mutable, traits::Watchable};

    #[test]
    fn test_buffer_time() {
        let source = Cell::new(0);
        let buffered = source
            .clone()
            .buffer_time(Duration::from_millis(50))
            .materialize();
        let (tx, rx) = std::sync::mpsc::channel::<Vec<i32>>();
        let _guard = buffered.subscribe(move |signal| {
            if let Signal::Value(v) = signal {
                let _ = tx.send((**v).clone());
            }
        });

        assert!(
            matches!(rx.recv_timeout(Duration::from_millis(200)), Ok(emitted) if emitted.is_empty()),
            "expected an empty initial emission"
        );
        source.set(1);
        source.set(2);
        source.set(3);
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            if let Ok(emitted) = rx.recv_timeout(Duration::from_millis(20))
                && emitted == vec![1, 2, 3]
            {
                break;
            }
            assert!(Instant::now() < deadline, "timed out waiting for buffer");
        }
    }

    #[test]
    fn test_buffer_time_emits_remainder_on_complete() {
        let source = Cell::new(0);
        let buffered = source
            .clone()
            .buffer_time(Duration::from_millis(100))
            .materialize();
        let (tx, rx) = std::sync::mpsc::channel::<Vec<i32>>();
        let _guard = buffered.subscribe(move |signal| {
            if let Signal::Value(v) = signal {
                let _ = tx.send((**v).clone());
            }
        });

        assert!(
            matches!(rx.recv_timeout(Duration::from_millis(200)), Ok(emitted) if emitted.is_empty()),
            "expected an empty initial emission"
        );
        source.set(1);
        source.set(2);
        source.complete();
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            if let Ok(emitted) = rx.recv_timeout(Duration::from_millis(20))
                && emitted == vec![1, 2]
            {
                break;
            }
            assert!(Instant::now() < deadline, "timed out waiting for remainder");
        }
    }
}
