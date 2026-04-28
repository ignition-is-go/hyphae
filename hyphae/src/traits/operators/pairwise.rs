//! `pairwise` operator — emit `(prev, current)` for each pair of consecutive values.
//!
//! [`Empty`] seedness: until the source has emitted twice, there is no pair to
//! produce, so the materialized cell starts as `None` and flips to
//! `Some((prev, current))` once the second emission lands.

use std::{marker::PhantomData, sync::Arc};

use arc_swap::ArcSwap;

use super::CellValue;
use crate::{
    pipeline::{Empty, MaterializeEmpty, Pipeline, PipelineInstall, PipelineSeed, Seedness},
    signal::Signal,
    subscription::SubscriptionGuard,
};

/// Pipeline node representing `source.pairwise()`.
pub struct PairwisePipeline<S, T, Sd = crate::pipeline::Definite> {
    source: S,
    _t: PhantomData<fn(T)>,
    _sd: PhantomData<fn(Sd)>,
}

impl<S, T, Sd> PipelineInstall<(T, T)> for PairwisePipeline<S, T, Sd>
where
    S: PipelineInstall<T> + PipelineSeed<T> + Send + Sync + 'static,
    Sd: Seedness,
    T: CellValue,
{
    fn install(&self, callback: Arc<dyn Fn(&Signal<(T, T)>) + Send + Sync>) -> SubscriptionGuard {
        // Seed `last` with source.seed(). The synchronous initial emit will
        // come in with the same value, so we want to swallow it (no pair yet).
        // Use a flag to detect the very first emission and store it without
        // emitting; subsequent emissions emit (prev, curr) and update prev.
        let last: Arc<ArcSwap<T>> = Arc::new(ArcSwap::from_pointee(self.source.seed()));
        let saw_first = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let wrapped: Arc<dyn Fn(&Signal<T>) + Send + Sync> = Arc::new(move |signal: &Signal<T>| {
            match signal {
                Signal::Value(v) => {
                    if !saw_first.swap(true, std::sync::atomic::Ordering::SeqCst) {
                        // First emission: just remember it, no pair to emit.
                        last.store(v.clone());
                        return;
                    }
                    let prev = last.load_full();
                    last.store(v.clone());
                    callback(&Signal::value(((*prev).clone(), v.as_ref().clone())));
                }
                Signal::Complete => callback(&Signal::Complete),
                Signal::Error(e) => callback(&Signal::Error(e.clone())),
            }
        });
        self.source.install(wrapped)
    }
}

#[allow(private_bounds)]
impl<S, T, Sd> Pipeline<(T, T), Empty> for PairwisePipeline<S, T, Sd>
where
    S: Pipeline<T, Sd> + PipelineSeed<T>,
    Sd: Seedness,
    T: CellValue,
{
}

impl<S, T, Sd> MaterializeEmpty<(T, T)> for PairwisePipeline<S, T, Sd>
where
    S: Pipeline<T, Sd> + PipelineSeed<T>,
    Sd: Seedness,
    T: CellValue,
{
}

#[allow(private_bounds)]
pub trait PairwiseExt<T: CellValue, S: Seedness>: Pipeline<T, S> + PipelineSeed<T> {
    /// Emit `(prev, current)` pairs for each consecutive pair of values.
    #[track_caller]
    fn pairwise(self) -> PairwisePipeline<Self, T, S> {
        PairwisePipeline {
            source: self,
            _t: PhantomData,
            _sd: PhantomData,
        }
    }
}

impl<T: CellValue, S: Seedness, P: Pipeline<T, S> + PipelineSeed<T>> PairwiseExt<T, S> for P {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Cell, Gettable, MaterializeEmpty, Mutable};

    #[test]
    fn test_pairwise_emits_pairs() {
        let source = Cell::new(1u64);
        let pairs = source.clone().pairwise().materialize();

        // No pair yet — first emission only stored.
        assert_eq!(pairs.get(), None);

        source.set(2);
        assert_eq!(pairs.get(), Some((1, 2)));

        source.set(3);
        assert_eq!(pairs.get(), Some((2, 3)));
    }
}
