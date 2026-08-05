//! `pairwise` operator — emit `(prev, current)` for each pair of consecutive values.
//!
//! [`Empty`] seedness: until the source has emitted twice, there is no pair to
//! produce, so the materialized cell starts as `None` and flips to
//! `Some((prev, current))` once the second emission lands.

use parking_lot::Mutex;
use std::{marker::PhantomData, sync::Arc};

use super::CellValue;
use crate::{
    pipeline::{Empty, Pipeline, PipelineInstall, Seedness},
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
    S: PipelineInstall<T> + Send + Sync + 'static,
    Sd: Seedness,
    T: CellValue,
{
    fn install(&self, callback: Arc<dyn Fn(&Signal<(T, T)>) + Send + Sync>) -> SubscriptionGuard {
        let last: Arc<Mutex<Option<T>>> = Arc::new(Mutex::new(None));
        let wrapped: Arc<dyn Fn(&Signal<T>) + Send + Sync> =
            Arc::new(move |signal: &Signal<T>| match signal {
                Signal::Value(v) => {
                    let mut guard = last.lock();
                    let Some(prev) = guard.replace((**v).clone()) else {
                        return;
                    };
                    drop(guard);
                    callback(&Signal::value((prev, v.as_ref().clone())));
                }
                Signal::Complete => callback(&Signal::Complete),
                Signal::Error(e) => callback(&Signal::Error(e.clone())),
            });
        self.source.install(wrapped)
    }
}

#[allow(private_bounds)]
impl<S, T, Sd> Pipeline<(T, T), Empty> for PairwisePipeline<S, T, Sd>
where
    S: Pipeline<T, Sd>,
    Sd: Seedness,
    T: CellValue,
{
}

#[allow(private_bounds)]
pub trait PairwiseExt<T: CellValue, S: Seedness>: Pipeline<T, S> {
    /// Emit `(prev, current)` pairs for each consecutive pair of values.
    #[track_caller]
    fn pairwise(self) -> impl crate::Materialize<(T, T), Empty> {
        PairwisePipeline {
            source: self,
            _t: PhantomData,
            _sd: PhantomData,
        }
    }
}

impl<T: CellValue, S: Seedness, P: Pipeline<T, S>> PairwiseExt<T, S> for P {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Cell, Gettable, Materialize, Mutable};

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
