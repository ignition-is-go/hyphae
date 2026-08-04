use std::{
    marker::PhantomData,
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
    },
};

use parking_lot::Mutex;

use super::{CellValue, Watchable};
use crate::{
    cell::{Cell, CellMutable},
    pipeline::{Definite, Pipeline, PipelineInstall, PipelineSeed, prepare_install},
    signal::Signal,
    subscription::SubscriptionGuard,
};

const LEFT_COMPLETE: u8 = 0b01;
const RIGHT_COMPLETE: u8 = 0b10;

/// Pipeline node that emits the latest pair whenever either source changes.
pub struct JoinPipeline<L, R, T, U> {
    left: L,
    right: R,
    _types: PhantomData<fn() -> (T, U)>,
}

impl<L, R, T, U> PipelineInstall<(T, U)> for JoinPipeline<L, R, T, U>
where
    L: PipelineInstall<T> + PipelineSeed<T>,
    R: PipelineInstall<U> + PipelineSeed<U>,
    T: CellValue,
    U: CellValue,
{
    fn install(&self, callback: Arc<dyn Fn(&Signal<(T, U)>) + Send + Sync>) -> SubscriptionGuard {
        let left_prepared = prepare_install(&self.left);
        let right_prepared = prepare_install(&self.right);
        let initial = (
            left_prepared.initial().clone(),
            right_prepared.initial().clone(),
        );
        let latest = Arc::new(Mutex::new(initial.clone()));
        let derived = Cell::<(T, U), CellMutable>::new(initial);
        let completed = Arc::new(AtomicU8::new(0));

        let left_latest = latest.clone();
        let left_completed = completed.clone();
        let left_weak = derived.downgrade();
        let left_guard = left_prepared.activate(Arc::new(move |signal| match signal {
            Signal::Value(value) => {
                let mut latest = left_latest.lock();
                latest.0 = value.as_ref().clone();
                if let Some(derived) = left_weak.upgrade() {
                    derived.notify(Signal::value(latest.clone()));
                }
            }
            Signal::Complete => {
                if left_completed.fetch_or(LEFT_COMPLETE, Ordering::SeqCst) == RIGHT_COMPLETE
                    && let Some(derived) = left_weak.upgrade()
                {
                    derived.notify(Signal::Complete);
                }
            }
            Signal::Error(error) => {
                if let Some(derived) = left_weak.upgrade() {
                    derived.notify(Signal::Error(error.clone()));
                }
            }
        }));
        derived.own(left_guard);

        let right_weak = derived.downgrade();
        let right_guard = right_prepared.activate(Arc::new(move |signal| match signal {
            Signal::Value(value) => {
                let mut latest = latest.lock();
                latest.1 = value.as_ref().clone();
                if let Some(derived) = right_weak.upgrade() {
                    derived.notify(Signal::value(latest.clone()));
                }
            }
            Signal::Complete => {
                if completed.fetch_or(RIGHT_COMPLETE, Ordering::SeqCst) == LEFT_COMPLETE
                    && let Some(derived) = right_weak.upgrade()
                {
                    derived.notify(Signal::Complete);
                }
            }
            Signal::Error(error) => {
                if let Some(derived) = right_weak.upgrade() {
                    derived.notify(Signal::Error(error.clone()));
                }
            }
        }));
        derived.own(right_guard);

        derived.subscribe(move |signal| callback(signal))
    }
}

impl<L, R, T, U> PipelineSeed<(T, U)> for JoinPipeline<L, R, T, U>
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

impl<L, R, T, U> Pipeline<(T, U), Definite> for JoinPipeline<L, R, T, U>
where
    L: Pipeline<T, Definite> + PipelineSeed<T>,
    R: Pipeline<U, Definite> + PipelineSeed<U>,
    T: CellValue,
    U: CellValue,
{
}

pub trait JoinExt<T: CellValue>: Pipeline<T, Definite> + PipelineSeed<T> {
    fn join<U, R>(self, other: R) -> impl crate::Materialize<(T, U), Definite>
    where
        U: CellValue,
        R: Pipeline<U, Definite> + PipelineSeed<U>,
    {
        JoinPipeline {
            left: self,
            right: other,
            _types: PhantomData,
        }
    }
}

impl<T: CellValue, P> JoinExt<T> for P where P: Pipeline<T, Definite> + PipelineSeed<T> {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Cell, Gettable, MapExt, Materialize, Mutable};

    #[test]
    fn test_join_combines_cells() {
        let a = Cell::new(1);
        let b = Cell::new("hello");
        let joined = a.clone().join(b.clone()).materialize();
        assert_eq!(joined.get(), (1, "hello"));
        a.set(2);
        assert_eq!(joined.get(), (2, "hello"));
        b.set("world");
        assert_eq!(joined.get(), (2, "world"));
    }

    #[test]
    fn test_flat_macro_chain() {
        let a = Cell::new(1);
        let b = Cell::new(2);
        let c = Cell::new(3);
        let d = Cell::new(4);
        let sum = a
            .join(b)
            .join(c)
            .join(d)
            .map(flat!(|a, b, c, d| a + b + c + d))
            .materialize();
        assert_eq!(sum.get(), 10);
    }
}
