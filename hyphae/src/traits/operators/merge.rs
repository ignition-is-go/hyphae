use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU8, Ordering},
};

use super::CellValue;
use crate::{
    pipeline::{Definite, Pipeline, PipelineInstall, PipelineSeed},
    signal::Signal,
    subscription::SubscriptionGuard,
};

const LEFT_COMPLETE: u8 = 0b01;
const RIGHT_COMPLETE: u8 = 0b10;

pub struct MergePipeline<L, R> {
    left: L,
    right: R,
}

impl<L, R, T> PipelineInstall<T> for MergePipeline<L, R>
where
    L: PipelineInstall<T>,
    R: PipelineInstall<T>,
    T: CellValue,
{
    fn install(&self, callback: Arc<dyn Fn(&Signal<T>) + Send + Sync>) -> SubscriptionGuard {
        let complete = Arc::new(AtomicU8::new(0));
        let left_complete = complete.clone();
        let left_callback = callback.clone();
        let first = AtomicBool::new(true);
        let left = self.left.install(Arc::new(move |signal| match signal {
            Signal::Value(_) if first.swap(false, Ordering::SeqCst) => {}
            Signal::Value(_) => left_callback(signal),
            Signal::Complete
                if left_complete.fetch_or(LEFT_COMPLETE, Ordering::SeqCst) == RIGHT_COMPLETE =>
            {
                left_callback(&Signal::Complete);
            }
            Signal::Complete => {}
            Signal::Error(error) => left_callback(&Signal::Error(error.clone())),
        }));
        // The right seed is intentionally emitted during installation: merge is
        // left-seeded, so the right current value is a real second event.
        let right = self.right.install(Arc::new(move |signal| match signal {
            Signal::Value(_) => callback(signal),
            Signal::Complete
                if complete.fetch_or(RIGHT_COMPLETE, Ordering::SeqCst) == LEFT_COMPLETE =>
            {
                callback(&Signal::Complete);
            }
            Signal::Complete => {}
            Signal::Error(error) => callback(&Signal::Error(error.clone())),
        }));
        SubscriptionGuard::combine(vec![left, right])
    }
}

impl<L, R, T> PipelineSeed<T> for MergePipeline<L, R>
where
    L: PipelineSeed<T>,
    R: PipelineInstall<T>,
    T: CellValue,
{
    fn seed(&self) -> T {
        self.left.seed()
    }
}

impl<L, R, T> Pipeline<T, Definite> for MergePipeline<L, R>
where
    L: Pipeline<T, Definite> + PipelineSeed<T>,
    R: Pipeline<T, Definite>,
    T: CellValue,
{
}

pub trait MergeExt<T: CellValue>: Pipeline<T, Definite> + PipelineSeed<T> {
    fn merge<R>(self, other: R) -> impl crate::Materialize<T, Definite>
    where
        R: Pipeline<T, Definite>,
    {
        MergePipeline {
            left: self,
            right: other,
        }
    }
}
impl<T: CellValue, P> MergeExt<T> for P where P: Pipeline<T, Definite> + PipelineSeed<T> {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Cell, Gettable, Materialize, Mutable};

    #[test]
    fn test_merge() {
        let a = Cell::new(1);
        let b = Cell::new(10);
        let merged = a.clone().merge(b.clone()).materialize();
        assert_eq!(merged.get(), 10);
        a.set(2);
        assert_eq!(merged.get(), 2);
        b.set(20);
        assert_eq!(merged.get(), 20);
        a.set(3);
        assert_eq!(merged.get(), 3);
    }
}
