use std::{
    marker::PhantomData,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

use crossbeam::queue::SegQueue;

use super::CellValue;
use crate::{
    pipeline::{Definite, MaterializeDefinite, Pipeline, PipelineInstall, PipelineSeed},
    signal::Signal,
    subscription::SubscriptionGuard,
};

/// Pipeline node representing `source.buffer_count(count)`.
pub struct BufferCountPipeline<S, T> {
    source: S,
    count: usize,
    _t: PhantomData<fn(T)>,
}

impl<S, T> PipelineInstall<Vec<T>> for BufferCountPipeline<S, T>
where
    S: PipelineInstall<T> + Send + Sync + 'static,
    T: CellValue,
{
    fn install(&self, callback: Arc<dyn Fn(&Signal<Vec<T>>) + Send + Sync>) -> SubscriptionGuard {
        let buffer = Arc::new(SegQueue::new());
        let buffer_len = Arc::new(AtomicUsize::new(0));
        let first = AtomicBool::new(true);
        let count = self.count;

        self.source.install(Arc::new(move |signal| match signal {
            Signal::Value(value) => {
                if first.swap(false, Ordering::SeqCst) {
                    return;
                }
                buffer.push(value.as_ref().clone());
                let len = buffer_len.fetch_add(1, Ordering::SeqCst) + 1;
                if len >= count {
                    let mut chunk = Vec::with_capacity(count);
                    for _ in 0..count {
                        if let Some(value) = buffer.pop() {
                            chunk.push(value);
                        }
                    }
                    buffer_len.fetch_sub(chunk.len(), Ordering::SeqCst);
                    callback(&Signal::value(chunk));
                }
            }
            Signal::Complete => {
                let mut remainder = Vec::new();
                while let Some(value) = buffer.pop() {
                    remainder.push(value);
                }
                if !remainder.is_empty() {
                    callback(&Signal::value(remainder));
                }
                callback(&Signal::Complete);
            }
            Signal::Error(error) => callback(&Signal::Error(error.clone())),
        }))
    }
}

impl<S, T> PipelineSeed<Vec<T>> for BufferCountPipeline<S, T>
where
    S: PipelineInstall<T>,
    T: CellValue,
{
    fn seed(&self) -> Vec<T> {
        Vec::new()
    }
}

#[allow(private_bounds)]
impl<S, T> Pipeline<Vec<T>, Definite> for BufferCountPipeline<S, T>
where
    S: Pipeline<T, Definite>,
    T: CellValue,
{
}

impl<S, T> MaterializeDefinite<Vec<T>> for BufferCountPipeline<S, T>
where
    S: Pipeline<T, Definite>,
    T: CellValue,
{
}

#[allow(private_bounds)]
pub trait BufferCountExt<T: CellValue>: Pipeline<T, Definite> {
    /// Collect values into non-overlapping chunks of size `count`.
    ///
    /// Emits a `Vec<T>` containing exactly `count` elements each time. On
    /// completion, emits any remaining buffered values.
    ///
    /// ```
    /// use hyphae::{BufferCountExt, Cell, MaterializeDefinite, Mutable};
    ///
    /// let source = Cell::new(0);
    /// let buffered = source.clone().buffer_count(3).materialize();
    /// source.set(1);
    /// source.set(2);
    /// source.set(3);
    /// ```
    #[track_caller]
    fn buffer_count(self, count: usize) -> BufferCountPipeline<Self, T> {
        assert!(count > 0, "buffer_count must be positive");
        BufferCountPipeline {
            source: self,
            count,
            _t: PhantomData,
        }
    }
}

impl<T: CellValue, P: Pipeline<T, Definite>> BufferCountExt<T> for P {}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use super::*;
    use crate::{Cell, MaterializeDefinite, Mutable, traits::Watchable};

    #[test]
    fn buffer_count_installs_only_when_materialized() {
        let source = Cell::new(0);
        let initial_subscribers = crate::traits::DepNode::subscriber_count(&source);
        let pipeline = source.clone().buffer_count(3);

        assert_eq!(
            crate::traits::DepNode::subscriber_count(&source),
            initial_subscribers
        );

        let _buffered = pipeline.materialize();
        assert_eq!(
            crate::traits::DepNode::subscriber_count(&source),
            initial_subscribers + 1
        );
    }

    #[test]
    fn test_buffer_count() {
        let source = Cell::new(0);
        let buffered = source.clone().buffer_count(3).materialize();
        let (tx, rx) = std::sync::mpsc::channel::<Vec<i32>>();

        let _guard = buffered.subscribe(move |signal| {
            if let Signal::Value(v) = signal {
                let _ = tx.send((**v).clone());
            }
        });

        assert_eq!(rx.recv().ok(), Some(vec![]));
        source.set(1);
        source.set(2);
        assert!(rx.try_recv().is_err());
        source.set(3);
        assert_eq!(rx.recv().ok(), Some(vec![1, 2, 3]));
        source.set(4);
        source.set(5);
        source.set(6);
        assert_eq!(rx.recv().ok(), Some(vec![4, 5, 6]));
    }

    #[test]
    fn test_buffer_count_emits_remainder_on_complete() {
        let source = Cell::new(0);
        let buffered = source.clone().buffer_count(3).materialize();
        let (tx, rx) = std::sync::mpsc::channel::<Vec<i32>>();
        let completed = Arc::new(AtomicUsize::new(0));

        let c = completed.clone();
        let _guard = buffered.subscribe(move |signal| match signal {
            Signal::Value(v) => {
                let _ = tx.send((**v).clone());
            }
            Signal::Complete => {
                c.fetch_add(1, Ordering::SeqCst);
            }
            _ => {}
        });

        source.set(1);
        source.set(2);
        assert_eq!(rx.recv().ok(), Some(vec![]));
        source.complete();
        assert_eq!(rx.recv().ok(), Some(vec![1, 2]));
        assert_eq!(completed.load(Ordering::SeqCst), 1);
    }
}
