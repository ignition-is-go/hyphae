use std::{
    marker::PhantomData,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

use super::{CellValue, Watchable};
use crate::{
    cell::{Cell, CellMutable},
    pipeline::{Definite, Pipeline, PipelineInstall, PipelineSeed},
    signal::Signal,
    subscription::SubscriptionGuard,
};

pub struct MergeMapPipeline<S, T, U, F, I> {
    source: S,
    f: Arc<F>,
    _types: PhantomData<fn(T) -> (U, I)>,
}

impl<S, T, U, F, I> PipelineInstall<U> for MergeMapPipeline<S, T, U, F, I>
where
    S: PipelineInstall<T> + PipelineSeed<T>,
    T: CellValue,
    U: CellValue,
    F: Fn(&T) -> I + Send + Sync + 'static,
    I: PipelineInstall<U> + PipelineSeed<U>,
{
    fn install(&self, callback: Arc<dyn Fn(&Signal<U>) + Send + Sync>) -> SubscriptionGuard {
        let first_inner = (self.f)(&self.source.seed());
        let cell = Cell::<U, CellMutable>::new(first_inner.seed());

        // Complete when outer completes AND all inner cells complete
        // Track: outer_complete flag + count of active (non-complete) inner cells
        let outer_complete = Arc::new(AtomicBool::new(false));
        let active_inners = Arc::new(AtomicUsize::new(1)); // Start with 1 for first_inner
        // One-shot guard so the terminal `Complete` fires exactly once. The
        // outer's completion and the final inner's completion are distinct
        // same-height cells that can run concurrently under the scheduler's
        // wave-parallel drain, and both can observe the "outer done AND no active
        // inners" condition — a non-atomic check-then-notify that would emit
        // `Complete` twice (confirmed by repro). Whoever wins this swap emits;
        // the other skips. (A lost completion is impossible — at least one side
        // observes the final state — so exactly-once holds.)
        let completed_emitted = Arc::new(AtomicBool::new(false));

        // Subscribe to first inner
        let weak = cell.downgrade();
        let oc = outer_complete.clone();
        let ai = active_inners.clone();
        let ce = completed_emitted.clone();
        let first_inner_guard = first_inner.install(Arc::new(move |signal| {
            if let Some(c) = weak.upgrade() {
                match signal {
                    Signal::Value(_) => c.notify(signal.clone()),
                    Signal::Complete => {
                        let remaining = ai.fetch_sub(1, Ordering::SeqCst).saturating_sub(1);
                        if remaining == 0
                            && oc.load(Ordering::SeqCst)
                            && !ce.swap(true, Ordering::SeqCst)
                        {
                            c.notify(Signal::Complete);
                        }
                    }
                    Signal::Error(e) => c.notify(Signal::Error(e.clone())),
                }
            }
        }));
        cell.own(first_inner_guard);

        // When outer changes, subscribe to new inner (without unsubscribing from previous)
        // Note: merge_map accumulates subscriptions by design - each inner cell stays subscribed
        let weak_outer = cell.downgrade();
        let f = self.f.clone();
        let first = Arc::new(AtomicBool::new(true));
        let oc2 = outer_complete.clone();
        let ai2 = active_inners.clone();
        let ce2 = completed_emitted;
        let outer_guard = self.source.install(Arc::new(move |signal| {
            match signal {
                Signal::Value(outer_value) => {
                    if first.swap(false, Ordering::SeqCst) {
                        return;
                    }

                    let Some(c) = weak_outer.upgrade() else {
                        return;
                    };

                    // Increment active count before creating inner
                    ai2.fetch_add(1, Ordering::SeqCst);

                    let inner = f(outer_value.as_ref());

                    // Subscribe to new inner - these subscriptions accumulate
                    let weak_inner = weak_outer.clone();
                    let oc_inner = oc2.clone();
                    let ai_inner = ai2.clone();
                    let ce_inner = ce2.clone();
                    let inner_guard = inner.install(Arc::new(move |signal| {
                        if let Some(c) = weak_inner.upgrade() {
                            match signal {
                                Signal::Value(_) => c.notify(signal.clone()),
                                Signal::Complete => {
                                    let remaining =
                                        ai_inner.fetch_sub(1, Ordering::SeqCst).saturating_sub(1);
                                    if remaining == 0
                                        && oc_inner.load(Ordering::SeqCst)
                                        && !ce_inner.swap(true, Ordering::SeqCst)
                                    {
                                        c.notify(Signal::Complete);
                                    }
                                }
                                Signal::Error(e) => c.notify(Signal::Error(e.clone())),
                            }
                        }
                    }));
                    c.own(inner_guard);
                }
                Signal::Complete => {
                    outer_complete.store(true, Ordering::SeqCst);
                    if active_inners.load(Ordering::SeqCst) == 0
                        && let Some(c) = weak_outer.upgrade()
                        && !ce2.swap(true, Ordering::SeqCst)
                    {
                        c.notify(Signal::Complete);
                    }
                }
                Signal::Error(e) => {
                    if let Some(c) = weak_outer.upgrade() {
                        c.notify(Signal::Error(e.clone()));
                    }
                }
            }
        }));
        cell.own(outer_guard);

        cell.subscribe(move |signal| callback(signal))
    }
}

impl<S, T, U, F, I> PipelineSeed<U> for MergeMapPipeline<S, T, U, F, I>
where
    S: PipelineSeed<T>,
    T: CellValue,
    U: CellValue,
    F: Fn(&T) -> I + Send + Sync + 'static,
    I: PipelineInstall<U> + PipelineSeed<U>,
{
    fn seed(&self) -> U {
        (self.f)(&self.source.seed()).seed()
    }
}
impl<S, T, U, F, I> Pipeline<U, Definite> for MergeMapPipeline<S, T, U, F, I>
where
    S: Pipeline<T, Definite> + PipelineSeed<T>,
    T: CellValue,
    U: CellValue,
    F: Fn(&T) -> I + Send + Sync + 'static,
    I: Pipeline<U, Definite> + PipelineSeed<U>,
{
}

pub trait MergeMapExt<T: CellValue>: Pipeline<T, Definite> + PipelineSeed<T> {
    fn merge_map<U, F, I>(self, f: F) -> impl crate::Materialize<U, Definite>
    where
        U: CellValue,
        F: Fn(&T) -> I + Send + Sync + 'static,
        I: Pipeline<U, Definite> + PipelineSeed<U>,
    {
        MergeMapPipeline {
            source: self,
            f: Arc::new(f),
            _types: PhantomData,
        }
    }
}
impl<T: CellValue, P> MergeMapExt<T> for P where P: Pipeline<T, Definite> + PipelineSeed<T> {}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::{Gettable, MapExt, Materialize};

    #[test]
    fn merge_map_does_not_build_an_inner_until_materialized() {
        let source = Cell::new(1u64);
        let calls = Arc::new(AtomicUsize::new(0));
        let inner_calls = calls.clone();
        let pipeline = source.merge_map(move |value| {
            inner_calls.fetch_add(1, Ordering::SeqCst);
            Cell::new(*value)
        });
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let _merged = pipeline.materialize();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn test_merge_map_merges() {
        let source = Cell::new(1u64);
        let merged = source
            .merge_map(|v| Cell::new(*v).map(|x| x * 10).materialize())
            .materialize();

        assert_eq!(merged.get(), 10);
    }
}
