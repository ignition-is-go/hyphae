use std::{
    marker::PhantomData,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use crossbeam::queue::ArrayQueue;

use super::CellValue;
use crate::{
    pipeline::{Definite, Pipeline, PipelineInstall, PipelineSeed},
    signal::Signal,
    subscription::SubscriptionGuard,
};

/// Pipeline node representing `source.drop_newest(capacity)`.
pub struct DropNewestPipeline<S, T> {
    source: S,
    capacity: usize,
    _t: PhantomData<fn(T)>,
}

impl<S, T> PipelineInstall<T> for DropNewestPipeline<S, T>
where
    S: PipelineInstall<T> + Send + Sync + 'static,
    T: CellValue,
{
    fn install(&self, callback: Arc<dyn Fn(&Signal<T>) + Send + Sync>) -> SubscriptionGuard {
        let buffer = Arc::new(ArrayQueue::new(self.capacity));
        let first = AtomicBool::new(true);

        self.source.install(Arc::new(move |signal| match signal {
            Signal::Value(value) => {
                // The materialized cell is initialized from `seed`; do not
                // count the source's synchronous initial replay against the
                // configured capacity.
                if first.swap(false, Ordering::SeqCst) {
                    return;
                }
                let value = value.as_ref().clone();
                if buffer.push(value.clone()).is_ok() {
                    callback(&Signal::value(value));
                }
            }
            Signal::Complete => callback(&Signal::Complete),
            Signal::Error(error) => callback(&Signal::Error(error.clone())),
        }))
    }
}

impl<S, T> PipelineSeed<T> for DropNewestPipeline<S, T>
where
    S: PipelineSeed<T>,
    T: CellValue,
{
    fn seed(&self) -> T {
        self.source.seed()
    }
}

#[allow(private_bounds)]
impl<S, T> Pipeline<T, Definite> for DropNewestPipeline<S, T>
where
    S: Pipeline<T, Definite> + PipelineSeed<T>,
    T: CellValue,
{
}

#[allow(private_bounds)]
pub trait BackpressureExt<T: CellValue>: Pipeline<T, Definite> + PipelineSeed<T> {
    /// Keep accepting values when capacity is reached.
    ///
    /// Hyphae pipelines deliver synchronously, so there is no independently
    /// draining consumer queue at this layer. Dropping an already-delivered
    /// "oldest" value cannot change the observable stream; this operator is
    /// therefore an allocation-free pass-through that retains the capacity
    /// check for API compatibility.
    #[track_caller]
    #[must_use]
    fn drop_oldest(self, capacity: usize) -> Self {
        assert!(capacity > 0, "capacity must be positive");
        self
    }

    /// Pass through the first `capacity` updates, then drop later updates.
    ///
    /// The queue state belongs to the installed pipeline subscription and no
    /// cell is allocated until the caller explicitly materializes the chain.
    #[track_caller]
    fn drop_newest(self, capacity: usize) -> impl crate::Materialize<T, Definite> {
        assert!(capacity > 0, "capacity must be positive");
        DropNewestPipeline {
            source: self,
            capacity,
            _t: PhantomData,
        }
    }

    /// Keep only the latest value.
    ///
    /// A materialized `Cell` already has latest-value semantics. At the
    /// pipeline layer this is exactly the identity operation, so it adds no
    /// intermediate state or subscription.
    #[track_caller]
    #[must_use]
    fn sample_latest(self) -> Self {
        self
    }
}

impl<T: CellValue, P: Pipeline<T, Definite> + PipelineSeed<T>> BackpressureExt<T> for P {}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;
    use crate::{Cell, Gettable, Materialize, Mutable, Signal, traits::Watchable};

    #[test]
    fn drop_oldest_is_lazy_passthrough() {
        let source = Cell::new(0);
        let initial_subscribers = crate::traits::DepNode::subscriber_count(&source);
        let pipeline = source.clone().drop_oldest(3);

        assert_eq!(
            crate::traits::DepNode::subscriber_count(&source),
            initial_subscribers
        );

        let buffered = pipeline.materialize();
        // Materializing a plain Cell remains a no-op because drop_oldest
        // returned the source pipeline unchanged.
        assert_eq!(
            crate::traits::DepNode::subscriber_count(&source),
            initial_subscribers
        );

        source.set(4);
        assert_eq!(buffered.get(), 4);
    }

    #[test]
    fn drop_newest_installs_only_when_materialized() {
        let source = Cell::new(0);
        let initial_subscribers = crate::traits::DepNode::subscriber_count(&source);
        let pipeline = source.clone().drop_newest(3);

        assert_eq!(
            crate::traits::DepNode::subscriber_count(&source),
            initial_subscribers
        );

        let buffered = pipeline.materialize();
        assert_eq!(
            crate::traits::DepNode::subscriber_count(&source),
            initial_subscribers + 1
        );

        let count = Arc::new(AtomicU32::new(0));
        let c = count.clone();
        let _guard = buffered.subscribe(move |_| {
            c.fetch_add(1, Ordering::SeqCst);
        });

        assert_eq!(count.load(Ordering::SeqCst), 1);
        source.set(1);
        source.set(2);
        source.set(3);
        assert_eq!(count.load(Ordering::SeqCst), 4);
        source.set(4);
        assert_eq!(count.load(Ordering::SeqCst), 4);
    }

    #[test]
    fn sample_latest_is_lazy_passthrough() {
        let source = Cell::new(0);
        let initial_subscribers = crate::traits::DepNode::subscriber_count(&source);
        let latest = source.clone().sample_latest().materialize();

        assert_eq!(
            crate::traits::DepNode::subscriber_count(&source),
            initial_subscribers
        );

        source.set(1);
        source.set(2);
        source.set(3);
        assert_eq!(latest.get(), 3);
    }

    #[test]
    fn drop_newest_forwards_complete() {
        let source = Cell::new(0);
        let buffered = source.clone().drop_newest(3).materialize();

        let completed = Arc::new(AtomicBool::new(false));
        let c = completed.clone();
        let _guard = buffered.subscribe(move |signal| {
            if matches!(signal, Signal::Complete) {
                c.store(true, Ordering::SeqCst);
            }
        });

        source.complete();
        assert!(completed.load(Ordering::SeqCst));
    }
}
