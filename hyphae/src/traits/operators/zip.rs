use std::{
    collections::VecDeque,
    marker::PhantomData,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use parking_lot::Mutex;

use super::CellValue;
use crate::{
    pipeline::{Definite, MaterializeDefinite, Pipeline, PipelineInstall, PipelineSeed},
    signal::Signal,
    subscription::SubscriptionGuard,
};

pub struct ZipPipeline<L, R, T, U> {
    left: L,
    right: R,
    _types: PhantomData<fn() -> (T, U)>,
}

impl<L, R, T, U> PipelineInstall<(T, U)> for ZipPipeline<L, R, T, U>
where
    L: PipelineInstall<T>,
    R: PipelineInstall<U>,
    T: CellValue,
    U: CellValue,
{
    fn install(&self, callback: Arc<dyn Fn(&Signal<(T, U)>) + Send + Sync>) -> SubscriptionGuard {
        let buffers = Arc::new(Mutex::new((VecDeque::<T>::new(), VecDeque::<U>::new())));
        let left_buffers = buffers.clone();
        let left_callback = callback.clone();
        let left_first = AtomicBool::new(true);
        let left = self.left.install(Arc::new(move |signal| match signal {
            Signal::Value(_) if left_first.swap(false, Ordering::SeqCst) => {}
            Signal::Value(value) => {
                let mut buffers = left_buffers.lock();
                if let Some(right) = buffers.1.pop_front() {
                    left_callback(&Signal::value((value.as_ref().clone(), right)));
                } else {
                    buffers.0.push_back(value.as_ref().clone());
                }
            }
            Signal::Complete => left_callback(&Signal::Complete),
            Signal::Error(error) => left_callback(&Signal::Error(error.clone())),
        }));
        let right_first = AtomicBool::new(true);
        let right = self.right.install(Arc::new(move |signal| match signal {
            Signal::Value(_) if right_first.swap(false, Ordering::SeqCst) => {}
            Signal::Value(value) => {
                let mut buffers = buffers.lock();
                if let Some(left) = buffers.0.pop_front() {
                    callback(&Signal::value((left, value.as_ref().clone())));
                } else {
                    buffers.1.push_back(value.as_ref().clone());
                }
            }
            Signal::Complete => callback(&Signal::Complete),
            Signal::Error(error) => callback(&Signal::Error(error.clone())),
        }));
        SubscriptionGuard::combine(vec![left, right])
    }
}

impl<L, R, T, U> PipelineSeed<(T, U)> for ZipPipeline<L, R, T, U>
where
    L: PipelineSeed<T>,
    R: PipelineSeed<U>,
    T: CellValue,
    U: CellValue,
{
    fn seed(&self) -> (T, U) {
        (self.left.seed(), self.right.seed())
    }
}
impl<L, R, T, U> Pipeline<(T, U), Definite> for ZipPipeline<L, R, T, U>
where
    L: Pipeline<T, Definite> + PipelineSeed<T>,
    R: Pipeline<U, Definite> + PipelineSeed<U>,
    T: CellValue,
    U: CellValue,
{
}
impl<L, R, T, U> MaterializeDefinite<(T, U)> for ZipPipeline<L, R, T, U>
where
    L: Pipeline<T, Definite> + PipelineSeed<T>,
    R: Pipeline<U, Definite> + PipelineSeed<U>,
    T: CellValue,
    U: CellValue,
{
}

pub trait ZipExt<T: CellValue>: Pipeline<T, Definite> + PipelineSeed<T> {
    fn zip<U, R>(self, other: R) -> ZipPipeline<Self, R, T, U>
    where
        U: CellValue,
        R: Pipeline<U, Definite> + PipelineSeed<U>,
    {
        ZipPipeline {
            left: self,
            right: other,
            _types: PhantomData,
        }
    }
}
impl<T: CellValue, P> ZipExt<T> for P where P: Pipeline<T, Definite> + PipelineSeed<T> {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Cell, Gettable, MaterializeDefinite, Mutable};
    #[test]
    fn test_zip() {
        let a = Cell::new(1);
        let b = Cell::new("a");
        let zipped = a.clone().zip(b.clone()).materialize();
        assert_eq!(zipped.get(), (1, "a"));
        a.set(2);
        a.set(3);
        assert_eq!(zipped.get(), (1, "a"));
        b.set("b");
        assert_eq!(zipped.get(), (2, "b"));
        b.set("c");
        assert_eq!(zipped.get(), (3, "c"));
    }
}
