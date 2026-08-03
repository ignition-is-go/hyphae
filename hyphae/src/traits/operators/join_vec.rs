use std::{
    marker::PhantomData,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

use parking_lot::Mutex;

use super::{CellValue, Watchable};
use crate::{
    cell::{Cell, CellMutable},
    pipeline::{Definite, MaterializeDefinite, Pipeline, PipelineInstall, PipelineSeed},
    signal::Signal,
    subscription::SubscriptionGuard,
};

pub struct JoinVecPipeline<P, T> {
    sources: Vec<P>,
    _type: PhantomData<fn() -> T>,
}

impl<P, T> PipelineInstall<Vec<T>> for JoinVecPipeline<P, T>
where
    P: PipelineInstall<T> + PipelineSeed<T>,
    T: CellValue,
{
    fn install(&self, callback: Arc<dyn Fn(&Signal<Vec<T>>) + Send + Sync>) -> SubscriptionGuard {
        if self.sources.is_empty() {
            callback(&Signal::Complete);
            return SubscriptionGuard::combine(vec![]);
        }
        let initial = self
            .sources
            .iter()
            .map(PipelineSeed::seed)
            .collect::<Vec<_>>();
        let latest = Arc::new(Mutex::new(initial.clone()));
        let derived = Cell::<Vec<T>, CellMutable>::new(initial);
        let completed = Arc::new(AtomicUsize::new(0));
        let count = self.sources.len();
        for (index, source) in self.sources.iter().enumerate() {
            let latest = latest.clone();
            let completed = completed.clone();
            let weak = derived.downgrade();
            let first = AtomicBool::new(true);
            let guard = source.install(Arc::new(move |signal| match signal {
                Signal::Value(_) if first.swap(false, Ordering::SeqCst) => {}
                Signal::Value(value) => {
                    let mut latest = latest.lock();
                    latest[index] = value.as_ref().clone();
                    if let Some(derived) = weak.upgrade() {
                        derived.notify(Signal::value(latest.clone()));
                    }
                }
                Signal::Complete if completed.fetch_add(1, Ordering::SeqCst) + 1 == count => {
                    if let Some(derived) = weak.upgrade() {
                        derived.notify(Signal::Complete);
                    }
                }
                Signal::Complete => {}
                Signal::Error(error) => {
                    if let Some(derived) = weak.upgrade() {
                        derived.notify(Signal::Error(error.clone()));
                    }
                }
            }));
            derived.own(guard);
        }
        derived.subscribe(move |signal| callback(signal))
    }
}
impl<P, T> PipelineSeed<Vec<T>> for JoinVecPipeline<P, T>
where
    P: PipelineSeed<T>,
    T: CellValue,
{
    fn seed(&self) -> Vec<T> {
        self.sources.iter().map(PipelineSeed::seed).collect()
    }
}
impl<P, T> Pipeline<Vec<T>, Definite> for JoinVecPipeline<P, T>
where
    P: Pipeline<T, Definite> + PipelineSeed<T>,
    T: CellValue,
{
}
impl<P, T> MaterializeDefinite<Vec<T>> for JoinVecPipeline<P, T>
where
    P: Pipeline<T, Definite> + PipelineSeed<T>,
    T: CellValue,
{
}

pub fn join_vec<T, P>(sources: Vec<P>) -> JoinVecPipeline<P, T>
where
    T: CellValue,
    P: Pipeline<T, Definite> + PipelineSeed<T>,
{
    JoinVecPipeline {
        sources,
        _type: PhantomData,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Cell, Gettable, MaterializeDefinite, Mutable, Watchable};
    #[test]
    fn test_join_vec_empty() {
        let combined = join_vec::<i32, Cell<i32, crate::CellImmutable>>(vec![]).materialize();
        assert_eq!(combined.get(), Vec::<i32>::new());
        assert!(combined.is_complete());
    }
    #[test]
    fn test_join_vec_multiple() {
        let a = Cell::new(1);
        let b = Cell::new(2);
        let c = Cell::new(3);
        let combined = join_vec(vec![a.clone(), b.clone(), c.clone()]).materialize();
        a.set(10);
        b.set(20);
        c.set(30);
        assert_eq!(combined.get(), vec![10, 20, 30]);
    }
}
