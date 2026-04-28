//! `cold` operator — drop the synchronous-on-subscribe initial emission.
//!
//! `cold()` produces a pipeline whose output is `Arc<T>` (cheap forwarding) and
//! whose materialized cell type is `Cell<Option<Arc<T>>>`, initialized to
//! `None`. The first source emission (the synchronous replay of the source's
//! current value) is swallowed; every subsequent emission lifts to
//! `Some(Arc<value>)`. Useful for trigger/event semantics where retained values
//! should not fire downstream effects.
//!
//! When used inside `switch_map`, each re-creation gets a fresh cold pipeline
//! (with its own first-skip), providing per-reconnection suppression of
//! retained values.

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

/// Pipeline node representing `source.cold()`. Output is `Arc<T>` so chains
/// stay zero-copy; materialize returns `Cell<Option<Arc<T>>>`.
pub struct ColdPipeline<S, T, Sd = crate::pipeline::Definite> {
    source: S,
    _t: PhantomData<fn(T)>,
    _sd: PhantomData<fn(Sd)>,
}

impl<S, T, Sd> PipelineInstall<Arc<T>> for ColdPipeline<S, T, Sd>
where
    S: PipelineInstall<T> + Send + Sync + 'static,
    Sd: Seedness,
    T: CellValue,
{
    fn install(
        &self,
        callback: Arc<dyn Fn(&Signal<Arc<T>>) + Send + Sync>,
    ) -> SubscriptionGuard {
        let first = Arc::new(AtomicBool::new(true));
        let wrapped: Arc<dyn Fn(&Signal<T>) + Send + Sync> =
            Arc::new(move |signal: &Signal<T>| match signal {
                Signal::Value(v) => {
                    if first.swap(false, Ordering::SeqCst) {
                        return;
                    }
                    // Forward as Signal<Arc<T>> by Arc-cloning the inner Arc.
                    callback(&Signal::value_arc(Arc::new(v.clone())));
                }
                Signal::Complete => callback(&Signal::Complete),
                Signal::Error(e) => callback(&Signal::Error(e.clone())),
            });
        self.source.install(wrapped)
    }
}

#[allow(private_bounds)]
impl<S, T, Sd> Pipeline<Arc<T>, Empty> for ColdPipeline<S, T, Sd>
where
    S: Pipeline<T, Sd>,
    Sd: Seedness,
    T: CellValue,
{
}

impl<S, T, Sd> MaterializeEmpty<Arc<T>> for ColdPipeline<S, T, Sd>
where
    S: Pipeline<T, Sd>,
    Sd: Seedness,
    T: CellValue,
{
}

#[allow(private_bounds)]
pub trait ColdExt<T: CellValue, S: Seedness>: Pipeline<T, S> {
    /// Drop the synchronous-on-subscribe initial emission; subsequent values
    /// flow through wrapped in `Arc` for cheap forwarding.
    ///
    /// Materializes to `Cell<Option<Arc<T>>>`, initialized to `None`. Once a
    /// post-subscribe emission arrives, the cell flips to `Some(Arc<value>)`
    /// and stays `Some` from then on (it tracks the most recent emission).
    #[track_caller]
    fn cold(self) -> ColdPipeline<Self, T, S> {
        ColdPipeline {
            source: self,
            _t: PhantomData,
            _sd: PhantomData,
        }
    }
}

impl<T: CellValue, S: Seedness, P: Pipeline<T, S>> ColdExt<T, S> for P {}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

    use super::*;
    use crate::{Cell, Gettable, MaterializeEmpty, Mutable, traits::Watchable};

    #[test]
    fn test_cold_starts_as_none() {
        let source = Cell::new(42u64);
        let cold = source.clone().cold().materialize();
        assert_eq!(cold.get(), None);
    }

    #[test]
    fn test_cold_emits_some_on_change() {
        let source = Cell::new(42u64);
        let cold = source.clone().cold().materialize();

        assert_eq!(cold.get(), None);

        source.set(100);
        assert_eq!(cold.get(), Some(Arc::new(100)));
    }

    #[test]
    fn test_cold_does_not_replay_retained_value() {
        let source = Cell::new(42u64);
        let cold = source.clone().cold().materialize();
        let emission_count = Arc::new(AtomicU64::new(0));

        let count = emission_count.clone();
        let _guard = cold.subscribe(move |signal| {
            if let Signal::Value(_) = signal {
                count.fetch_add(1, AtomicOrdering::SeqCst);
            }
        });

        // Subscribe fires once with initial None value
        assert_eq!(emission_count.load(AtomicOrdering::SeqCst), 1);
        assert_eq!(cold.get(), None); // retained source value (42) was NOT replayed

        source.set(100);
        assert_eq!(emission_count.load(AtomicOrdering::SeqCst), 2);
        assert_eq!(cold.get(), Some(Arc::new(100)));

        source.set(200);
        assert_eq!(emission_count.load(AtomicOrdering::SeqCst), 3);
        assert_eq!(cold.get(), Some(Arc::new(200)));
    }
}
