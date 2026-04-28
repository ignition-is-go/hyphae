//! `finalize` operator — pure terminal callback; chains fuse into one closure on root.
//!
//! Forwards every value untransformed and runs `callback` exactly once when
//! the stream emits a `Complete` or `Error` signal.

use std::{
    cell::UnsafeCell,
    marker::PhantomData,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use super::CellValue;
use crate::{
    pipeline::{
        Definite, Empty, MaterializeDefinite, MaterializeEmpty, Pipeline, PipelineInstall,
        PipelineSeed, Seedness,
    },
    signal::Signal,
    subscription::SubscriptionGuard,
};

/// A callback that can only be called once, implemented lock-free.
struct OnceCallback<F> {
    called: AtomicBool,
    callback: UnsafeCell<Option<F>>,
}

// Safety: the atomic bool ensures only one thread can access the callback.
unsafe impl<F: Send> Send for OnceCallback<F> {}
unsafe impl<F: Send> Sync for OnceCallback<F> {}

impl<F: FnOnce()> OnceCallback<F> {
    fn new(f: F) -> Self {
        Self {
            called: AtomicBool::new(false),
            callback: UnsafeCell::new(Some(f)),
        }
    }

    fn call(&self) {
        if !self.called.swap(true, Ordering::SeqCst) {
            // Safety: called is now true, so no other thread enters this block.
            unsafe {
                if let Some(cb) = (*self.callback.get()).take() {
                    cb();
                }
            }
        }
    }
}

/// Pipeline node representing `source.finalize(f)`.
pub struct FinalizePipeline<S, T, F, Sd = Definite> {
    source: S,
    callback: Arc<OnceCallback<F>>,
    _t: PhantomData<fn(T)>,
    _sd: PhantomData<fn(Sd)>,
}

impl<S, T, F, Sd> PipelineInstall<T> for FinalizePipeline<S, T, F, Sd>
where
    S: PipelineInstall<T> + Send + Sync + 'static,
    Sd: Seedness,
    T: CellValue,
    F: FnOnce() + Send + Sync + 'static,
{
    fn install(&self, callback: Arc<dyn Fn(&Signal<T>) + Send + Sync>) -> SubscriptionGuard {
        let oncecb = Arc::clone(&self.callback);
        let wrapped: Arc<dyn Fn(&Signal<T>) + Send + Sync> =
            Arc::new(move |signal: &Signal<T>| match signal {
                Signal::Value(_) => callback(signal),
                Signal::Complete => {
                    oncecb.call();
                    callback(signal);
                }
                Signal::Error(_) => {
                    oncecb.call();
                    callback(signal);
                }
            });
        self.source.install(wrapped)
    }
}

impl<S, T, F> PipelineSeed<T> for FinalizePipeline<S, T, F, Definite>
where
    S: PipelineSeed<T>,
    T: CellValue,
    F: FnOnce() + Send + Sync + 'static,
{
    fn seed(&self) -> T {
        self.source.seed()
    }
}

#[allow(private_bounds)]
impl<S, T, F, Sd> Pipeline<T, Sd> for FinalizePipeline<S, T, F, Sd>
where
    S: Pipeline<T, Sd>,
    Sd: Seedness,
    T: CellValue,
    F: FnOnce() + Send + Sync + 'static,
{
}

impl<S, T, F> MaterializeDefinite<T> for FinalizePipeline<S, T, F, Definite>
where
    S: Pipeline<T, Definite> + PipelineSeed<T>,
    T: CellValue,
    F: FnOnce() + Send + Sync + 'static,
{
}

impl<S, T, F> MaterializeEmpty<T> for FinalizePipeline<S, T, F, Empty>
where
    S: Pipeline<T, Empty>,
    T: CellValue,
    F: FnOnce() + Send + Sync + 'static,
{
}

#[allow(private_bounds)]
pub trait FinalizeExt<T: CellValue, S: Seedness>: Pipeline<T, S> {
    /// Execute a callback exactly once when the stream completes or errors.
    ///
    /// Values pass through untransformed.
    ///
    /// # Example
    ///
    /// ```
    /// use hyphae::{Cell, FinalizeExt, MaterializeDefinite, Mutable};
    /// use std::sync::Arc;
    /// use std::sync::atomic::{AtomicBool, Ordering};
    ///
    /// let source = Cell::new(0);
    /// let finalized_flag = Arc::new(AtomicBool::new(false));
    /// let flag = finalized_flag.clone();
    ///
    /// let finalized = source.clone().finalize(move || {
    ///     flag.store(true, Ordering::SeqCst);
    /// }).materialize();
    ///
    /// source.set(1);
    /// source.complete();
    /// assert!(finalized_flag.load(Ordering::SeqCst));
    /// ```
    #[track_caller]
    fn finalize<F>(self, callback: F) -> FinalizePipeline<Self, T, F, S>
    where
        F: FnOnce() + Send + Sync + 'static,
    {
        FinalizePipeline {
            source: self,
            callback: Arc::new(OnceCallback::new(callback)),
            _t: PhantomData,
            _sd: PhantomData,
        }
    }
}

impl<T: CellValue, S: Seedness, P: Pipeline<T, S>> FinalizeExt<T, S> for P {}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};

    use super::*;
    use crate::{Cell, Gettable, MaterializeDefinite, Mutable};

    #[test]
    fn test_finalize_on_complete() {
        let source = Cell::new(0);
        let finalized = Arc::new(AtomicBool::new(false));

        let f = finalized.clone();
        let _finalized_cell = source
            .clone()
            .finalize(move || {
                f.store(true, Ordering::SeqCst);
            })
            .materialize();

        assert!(!finalized.load(Ordering::SeqCst));

        source.complete();
        assert!(finalized.load(Ordering::SeqCst));
    }

    #[test]
    fn test_finalize_on_error() {
        let source = Cell::new(0);
        let finalized = Arc::new(AtomicBool::new(false));

        let f = finalized.clone();
        let _finalized_cell = source
            .clone()
            .finalize(move || {
                f.store(true, Ordering::SeqCst);
            })
            .materialize();

        assert!(!finalized.load(Ordering::SeqCst));

        source.fail(anyhow::anyhow!("error"));
        assert!(finalized.load(Ordering::SeqCst));
    }

    #[test]
    fn test_finalize_called_once() {
        let source = Cell::new(0);
        let count = Arc::new(AtomicU32::new(0));

        let c = count.clone();
        let _finalized_cell = source
            .clone()
            .finalize(move || {
                c.fetch_add(1, AtomicOrdering::SeqCst);
            })
            .materialize();

        source.complete();
        source.complete(); // second complete
        assert_eq!(count.load(AtomicOrdering::SeqCst), 1); // only called once
    }

    #[test]
    fn test_finalize_passes_values_through() {
        let source = Cell::new(5);
        let finalized = source.clone().finalize(|| {}).materialize();
        assert_eq!(finalized.get(), 5);
        source.set(42);
        assert_eq!(finalized.get(), 42);
    }
}
