use std::{
    collections::VecDeque,
    marker::PhantomData,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use parking_lot::Mutex;

use super::{
    CellValue,
    fan_in::{Arrival, fan_in},
};
use crate::{
    pipeline::{Definite, Pipeline, PipelineInstall, PipelineSeed},
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
        let buffers = Mutex::new((VecDeque::<T>::new(), VecDeque::<U>::new()));
        let left_first = AtomicBool::new(true);
        let right_first = AtomicBool::new(true);
        fan_in(&self.left, &self.right, move |arrival| match arrival {
            Arrival::Left(Signal::Value(_)) if left_first.swap(false, Ordering::SeqCst) => {}
            Arrival::Right(Signal::Value(_)) if right_first.swap(false, Ordering::SeqCst) => {}
            Arrival::Left(Signal::Value(value)) => {
                let right = {
                    let mut buffers = buffers.lock();
                    let right = buffers.1.pop_front();
                    if right.is_none() {
                        buffers.0.push_back(value.as_ref().clone());
                    }
                    right
                };
                if let Some(right) = right {
                    callback(&Signal::value((value.as_ref().clone(), right)));
                }
            }
            Arrival::Right(Signal::Value(value)) => {
                let left = {
                    let mut buffers = buffers.lock();
                    let left = buffers.0.pop_front();
                    if left.is_none() {
                        buffers.1.push_back(value.as_ref().clone());
                    }
                    left
                };
                if let Some(left) = left {
                    callback(&Signal::value((left, value.as_ref().clone())));
                }
            }
            Arrival::Left(Signal::Complete) | Arrival::Right(Signal::Complete) => {
                callback(&Signal::Complete);
            }
            Arrival::Left(Signal::Error(error)) | Arrival::Right(Signal::Error(error)) => {
                callback(&Signal::Error(error));
            }
        })
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

pub trait ZipExt<T: CellValue>: Pipeline<T, Definite> + PipelineSeed<T> {
    fn zip<U, R>(self, other: R) -> impl crate::Materialize<(T, U), Definite>
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
    use crate::{Cell, Gettable, Materialize, Mutable};
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
