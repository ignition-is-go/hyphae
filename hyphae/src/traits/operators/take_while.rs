//! `take_while(p)` operator — emit values while predicate returns true, then complete.
//!
//! [`Empty`] seedness: the predicate may fail on the first emission, in which
//! case nothing ever lands in the cell and it stays `None` forever.

use std::{
    marker::PhantomData,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use super::CellValue;
use crate::{
    pipeline::{Empty, MaterializeEmpty, Pipeline, PipelineInstall, Seedness},
    signal::Signal,
    subscription::SubscriptionGuard,
};

/// Pipeline node representing `source.take_while(p)`.
pub struct TakeWhilePipeline<S, T, F, Sd = crate::pipeline::Definite> {
    source: S,
    predicate: Arc<F>,
    _t: PhantomData<fn(T)>,
    _sd: PhantomData<fn(Sd)>,
}

impl<S, T, F, Sd> PipelineInstall<T> for TakeWhilePipeline<S, T, F, Sd>
where
    S: PipelineInstall<T> + Send + Sync + 'static,
    Sd: Seedness,
    T: CellValue,
    F: Fn(&T) -> bool + Send + Sync + 'static,
{
    fn install(&self, callback: Arc<dyn Fn(&Signal<T>) + Send + Sync>) -> SubscriptionGuard {
        let predicate = Arc::clone(&self.predicate);
        let stopped = Arc::new(AtomicBool::new(false));
        let wrapped: Arc<dyn Fn(&Signal<T>) + Send + Sync> =
            Arc::new(move |signal: &Signal<T>| match signal {
                Signal::Value(v) => {
                    if stopped.load(Ordering::SeqCst) {
                        return;
                    }
                    if predicate(v.as_ref()) {
                        callback(signal);
                    } else {
                        stopped.store(true, Ordering::SeqCst);
                        callback(&Signal::Complete);
                    }
                }
                Signal::Complete => callback(&Signal::Complete),
                Signal::Error(e) => callback(&Signal::Error(e.clone())),
            });
        self.source.install(wrapped)
    }
}

#[allow(private_bounds)]
impl<S, T, F, Sd> Pipeline<T, Empty> for TakeWhilePipeline<S, T, F, Sd>
where
    S: Pipeline<T, Sd>,
    Sd: Seedness,
    T: CellValue,
    F: Fn(&T) -> bool + Send + Sync + 'static,
{
}

impl<S, T, F, Sd> MaterializeEmpty<T> for TakeWhilePipeline<S, T, F, Sd>
where
    S: Pipeline<T, Sd>,
    Sd: Seedness,
    T: CellValue,
    F: Fn(&T) -> bool + Send + Sync + 'static,
{
}

#[allow(private_bounds)]
pub trait TakeWhileExt<T: CellValue, S: Seedness>: Pipeline<T, S> {
    /// Forward values while the predicate returns `true`. On the first `false`,
    /// emit `Complete` and ignore all subsequent values.
    #[track_caller]
    fn take_while<F>(self, predicate: F) -> TakeWhilePipeline<Self, T, F, S>
    where
        F: Fn(&T) -> bool + Send + Sync + 'static,
    {
        TakeWhilePipeline {
            source: self,
            predicate: Arc::new(predicate),
            _t: PhantomData,
            _sd: PhantomData,
        }
    }
}

impl<T: CellValue, S: Seedness, P: Pipeline<T, S>> TakeWhileExt<T, S> for P {}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;

    use super::*;
    use crate::{Cell, Gettable, MaterializeEmpty, Mutable, traits::Watchable};

    #[test]
    fn test_take_while() {
        let source = Cell::new(1u64);
        let taken = source.clone().take_while(|x| *x < 5).materialize();

        // Initial sync emit: predicate(1) == true, cell becomes Some(1).
        assert_eq!(taken.get(), Some(1));

        source.set(3);
        assert_eq!(taken.get(), Some(3));

        source.set(5); // predicate fails, completes; cell freezes at last passing value.
        assert_eq!(taken.get(), Some(3));

        source.set(2); // already stopped — ignored.
        assert_eq!(taken.get(), Some(3));
    }

    #[test]
    fn test_take_while_completes_on_predicate_fail() {
        let source = Cell::new(1u64);
        let taken = source.clone().take_while(|x| *x < 5).materialize();
        let completed = Arc::new(AtomicBool::new(false));

        let c = completed.clone();
        let _guard = taken.subscribe(move |signal| {
            if let Signal::Complete = signal {
                c.store(true, Ordering::SeqCst);
            }
        });

        assert!(!taken.is_complete());

        source.set(5); // predicate fails

        assert!(taken.is_complete());
        assert!(completed.load(Ordering::SeqCst));
    }
}
