//! `take(n)` operator — emit up to `n` values, then complete.
//!
//! [`Definite`] when source is `Definite`: the synchronous initial emission
//! counts as the first of `n` (matching the historical cell-based behavior),
//! so `take(1)` materializes to a `Cell<T>` holding the source's initial value
//! and immediately complete.

use std::{cell::RefCell, marker::PhantomData, sync::Arc};

use parking_lot::ReentrantMutex;

use super::CellValue;
use crate::{
    pipeline::{Definite, Pipeline, PipelineInstall, PipelineSeed, Seedness},
    signal::Signal,
    subscription::SubscriptionGuard,
};

/// Pipeline node representing `source.take(n)`.
pub struct TakePipeline<S, T, Sd = Definite> {
    source: S,
    count: usize,
    _t: PhantomData<fn(T)>,
    _sd: PhantomData<fn(Sd)>,
}

impl<S, T, Sd> PipelineInstall<T> for TakePipeline<S, T, Sd>
where
    S: PipelineInstall<T> + Send + Sync + 'static,
    Sd: Seedness,
    T: CellValue,
{
    fn install(&self, callback: Arc<dyn Fn(&Signal<T>) + Send + Sync>) -> SubscriptionGuard {
        // `n` counts emissions downstream. The output cell drops a value equal
        // to the one it holds, so a repeat of the last forwarded value is not
        // an emission and must not consume the budget. Deciding and forwarding
        // under one lock keeps the forwarded order identical to the decision
        // order, so the output cell's last value is always `last` here, even
        // when concurrent writers reach this stage out of order. The lock is
        // reentrant so a subscriber may write the source synchronously.
        let state = Arc::new(ReentrantMutex::new(RefCell::new(TakeState::<T> {
            remaining: self.count,
            last: None,
        })));
        let wrapped: Arc<dyn Fn(&Signal<T>) + Send + Sync> =
            Arc::new(move |signal: &Signal<T>| match signal {
                Signal::Value(value) => {
                    let guard = state.lock();
                    let completes = {
                        let mut state = guard.borrow_mut();
                        if state.last.as_ref() == Some(&**value) {
                            return;
                        }
                        let Some(remaining) = state.remaining.checked_sub(1) else {
                            return;
                        };
                        state.remaining = remaining;
                        state.last = Some((**value).clone());
                        remaining == 0
                    };
                    callback(signal);
                    if completes {
                        callback(&Signal::Complete);
                    }
                    drop(guard);
                }
                Signal::Complete => callback(&Signal::Complete),
                Signal::Error(e) => callback(&Signal::Error(e.clone())),
            });
        self.source.install(wrapped)
    }
}

struct TakeState<T> {
    remaining: usize,
    last: Option<T>,
}

impl<S, T, Sd> PipelineSeed<T> for TakePipeline<S, T, Sd>
where
    S: Pipeline<T, crate::pipeline::Definite>,
    Sd: Seedness,
    T: CellValue,
{
    fn seed(&self) -> T {
        self.source.pipeline_seed()
    }
}

#[allow(private_bounds)]
impl<S, T> Pipeline<T, crate::pipeline::Definite> for TakePipeline<S, T, crate::pipeline::Definite>
where
    S: Pipeline<T, crate::pipeline::Definite>,
    T: CellValue,
{
}

impl<S, T> Pipeline<T, crate::pipeline::Empty> for TakePipeline<S, T, crate::pipeline::Empty>
where
    S: Pipeline<T, crate::pipeline::Empty>,
    T: CellValue,
{
}

#[allow(private_bounds)]
pub trait TakeExt<T: CellValue, S: Seedness>: Pipeline<T, S> {
    #[track_caller]
    fn take(self, count: usize) -> impl crate::Materialize<T, S>;
}

impl<T: CellValue, P: Pipeline<T, crate::pipeline::Definite>> TakeExt<T, crate::pipeline::Definite>
    for P
{
    fn take(self, count: usize) -> impl crate::Materialize<T, crate::pipeline::Definite> {
        TakePipeline {
            source: self,
            count,
            _t: PhantomData,
            _sd: PhantomData,
        }
    }
}

impl<T: CellValue, P: Pipeline<T, crate::pipeline::Empty>> TakeExt<T, crate::pipeline::Empty>
    for P
{
    fn take(self, count: usize) -> impl crate::Materialize<T, crate::pipeline::Empty> {
        TakePipeline {
            source: self,
            count,
            _t: PhantomData,
            _sd: PhantomData,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering as AtomicOrdering},
    };

    use super::*;
    use crate::{Cell, Gettable, Materialize, Mutable, traits::Watchable};

    #[test]
    fn test_take_limits_emissions() {
        let source = Cell::new(0u64);
        let taken = source.clone().take(3).materialize();
        let count = Arc::new(AtomicU64::new(0));

        let c = count.clone();
        let _guard = taken.subscribe(move |signal| {
            if let Signal::Value(_) = signal {
                c.fetch_add(1, AtomicOrdering::SeqCst);
            }
        });

        // Initial sync emit counts as the 1st take.
        assert_eq!(count.load(AtomicOrdering::SeqCst), 1);

        source.set(1); // 2nd
        source.set(2); // 3rd, completes
        source.set(3); // ignored
        source.set(4); // ignored

        assert_eq!(count.load(AtomicOrdering::SeqCst), 3);
    }

    #[test]
    fn test_take_completes() {
        let source = Cell::new(0u64);
        let taken = source.clone().take(2).materialize();
        let completed = Arc::new(AtomicBool::new(false));

        let c = completed.clone();
        let _guard = taken.subscribe(move |signal| {
            if matches!(signal, Signal::Complete) {
                c.store(true, AtomicOrdering::SeqCst);
            }
        });

        assert!(!taken.is_complete());
        assert!(!completed.load(AtomicOrdering::SeqCst));

        source.set(1); // 2nd emission, completes

        assert!(taken.is_complete());
        assert!(completed.load(AtomicOrdering::SeqCst));
    }

    /// An upstream that repeats a value (here `map`, which does not dedupe)
    /// hands `take` an input the output cell drops. That input is not an
    /// emission and must not count, or `take(3)` completes after two values.
    #[test]
    fn test_take_ignores_repeats_of_last_emission() {
        use crate::MapExt;

        let source = Cell::new(0u64);
        let taken = source.clone().map(|v| v / 2).take(3).materialize();
        let seen = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let s = seen.clone();
        let _guard = taken.subscribe(move |signal| s.lock().push(signal.clone()));

        for i in 1..8 {
            source.set(i);
        }

        let seen = seen.lock().clone();
        let values: Vec<u64> = seen
            .iter()
            .filter_map(|signal| match signal {
                Signal::Value(v) => Some(**v),
                _ => None,
            })
            .collect();
        assert_eq!(values, vec![0, 1, 2]);
        assert!(matches!(seen.last(), Some(Signal::Complete)));
        assert_eq!(taken.get(), 2);
    }

    /// A subscriber may write the source synchronously from inside a `take`
    /// emission without deadlocking, and the nested emission still counts.
    #[test]
    fn test_take_allows_reentrant_source_writes() {
        let source = Cell::new(0u64);
        let taken = source.clone().take(3).materialize();
        let count = Arc::new(AtomicU64::new(0));
        let c = count.clone();
        let writer = source;
        let _guard = taken.subscribe(move |signal| {
            if let Signal::Value(v) = signal {
                c.fetch_add(1, AtomicOrdering::SeqCst);
                if **v < 5 {
                    writer.set(**v + 1);
                }
            }
        });
        assert_eq!(count.load(AtomicOrdering::SeqCst), 3);
        assert_eq!(taken.get(), 2);
    }
}
