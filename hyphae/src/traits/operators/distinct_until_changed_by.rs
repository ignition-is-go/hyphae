//! `distinct_until_changed_by` operator — predicate-gated dedupe.
//!
//! Emits when the user-supplied comparator returns `false` (values differ).
//! First emission has no prior value to compare against, so it always passes —
//! the operator is therefore [`Definite`] when its source is `Definite`.

use parking_lot::Mutex;
use std::{marker::PhantomData, sync::Arc};

use super::CellValue;
use crate::{
    pipeline::{Definite, Pipeline, PipelineInstall, PipelineSeed, Seedness},
    signal::Signal,
    subscription::SubscriptionGuard,
};

/// Pipeline node representing `source.distinct_until_changed_by(cmp)`.
pub struct DistinctUntilChangedByPipeline<S, T, F, Sd = Definite> {
    source: S,
    comparator: Arc<F>,
    _t: PhantomData<fn(T)>,
    _sd: PhantomData<fn(Sd)>,
}

impl<S, T, F, Sd> PipelineInstall<T> for DistinctUntilChangedByPipeline<S, T, F, Sd>
where
    S: PipelineInstall<T> + Send + Sync + 'static,
    Sd: Seedness,
    T: CellValue,
    F: Fn(&T, &T) -> bool + Send + Sync + 'static,
{
    fn install(&self, callback: Arc<dyn Fn(&Signal<T>) + Send + Sync>) -> SubscriptionGuard {
        let comparator = Arc::clone(&self.comparator);
        // `Mutex<T>` (not `ArcSwap<T>`): the install closure runs serially
        // from a single upstream notify thread, so we don't need arc_swap's
        // multi-reader debt machinery. Uncontended `Mutex::lock` is one
        // CAS; the prior value drops inline at end-of-scope.
        let last_value: Arc<Mutex<Option<T>>> = Arc::new(Mutex::new(None));
        let wrapped: Arc<dyn Fn(&Signal<T>) + Send + Sync> =
            Arc::new(move |signal: &Signal<T>| match signal {
                Signal::Value(v) => {
                    let mut last = last_value.lock();
                    let differs = last
                        .as_ref()
                        .is_none_or(|previous| !(comparator)(v.as_ref(), previous));
                    if differs {
                        *last = Some((**v).clone());
                        // Release the lock before invoking the callback to
                        // avoid holding it across user code.
                        drop(last);
                        callback(signal);
                    }
                }
                Signal::Complete => callback(&Signal::Complete),
                Signal::Error(e) => callback(&Signal::Error(e.clone())),
            });
        self.source.install(wrapped)
    }
}

impl<S, T, F, Sd> PipelineSeed<T> for DistinctUntilChangedByPipeline<S, T, F, Sd>
where
    S: Pipeline<T, crate::pipeline::Definite>,
    Sd: Seedness,
    T: CellValue,
    F: Fn(&T, &T) -> bool + Send + Sync + 'static,
{
    fn seed(&self) -> T {
        self.source.pipeline_seed()
    }
}

#[allow(private_bounds)]
impl<S, T, F> Pipeline<T, crate::pipeline::Definite>
    for DistinctUntilChangedByPipeline<S, T, F, crate::pipeline::Definite>
where
    S: Pipeline<T, crate::pipeline::Definite>,
    T: CellValue,
    F: Fn(&T, &T) -> bool + Send + Sync + 'static,
{
}

impl<S, T, F> Pipeline<T, crate::pipeline::Empty>
    for DistinctUntilChangedByPipeline<S, T, F, crate::pipeline::Empty>
where
    S: Pipeline<T, crate::pipeline::Empty>,
    T: CellValue,
    F: Fn(&T, &T) -> bool + Send + Sync + 'static,
{
}

#[allow(private_bounds)]
pub trait DistinctUntilChangedByExt<T: CellValue, S: Seedness>: Pipeline<T, S> {
    /// Like `deduped()` but with a custom comparator.
    ///
    /// Only emits when the comparator returns `false` (values differ).
    ///
    /// # Example
    ///
    /// ```
    /// use hyphae::{Cell, DistinctUntilChangedByExt, Materialize, Mutable};
    ///
    /// #[derive(Clone, Debug, PartialEq)]
    /// struct User { id: u32, name: String }
    ///
    /// let source = Cell::new(User { id: 1, name: "Alice".into() });
    /// let by_id = source.clone().distinct_until_changed_by(|a, b| a.id == b.id).materialize();
    ///
    /// source.set(User { id: 1, name: "Alicia".into() }); // same id - blocked
    /// source.set(User { id: 2, name: "Bob".into() });    // different id - passes
    /// ```
    #[track_caller]
    fn distinct_until_changed_by<F>(self, comparator: F) -> impl crate::Materialize<T, S>
    where
        F: Fn(&T, &T) -> bool + Send + Sync + 'static;
}

impl<T: CellValue, P: Pipeline<T, crate::pipeline::Definite>>
    DistinctUntilChangedByExt<T, crate::pipeline::Definite> for P
{
    fn distinct_until_changed_by<F>(
        self,
        comparator: F,
    ) -> impl crate::Materialize<T, crate::pipeline::Definite>
    where
        F: Fn(&T, &T) -> bool + Send + Sync + 'static,
    {
        DistinctUntilChangedByPipeline {
            source: self,
            comparator: Arc::new(comparator),
            _t: PhantomData,
            _sd: PhantomData,
        }
    }
}

impl<T: CellValue, P: Pipeline<T, crate::pipeline::Empty>>
    DistinctUntilChangedByExt<T, crate::pipeline::Empty> for P
{
    fn distinct_until_changed_by<F>(
        self,
        comparator: F,
    ) -> impl crate::Materialize<T, crate::pipeline::Empty>
    where
        F: Fn(&T, &T) -> bool + Send + Sync + 'static,
    {
        DistinctUntilChangedByPipeline {
            source: self,
            comparator: Arc::new(comparator),
            _t: PhantomData,
            _sd: PhantomData,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    };

    use super::*;
    use crate::{Cell, Materialize, Mutable, traits::Watchable};

    #[derive(Clone, Debug, PartialEq)]
    struct User {
        id: u32,
        #[allow(dead_code)]
        name: String,
    }

    #[test]
    fn test_distinct_until_changed_by() {
        let source = Cell::new(User {
            id: 1,
            name: "Alice".into(),
        });
        let by_id = source
            .clone()
            .distinct_until_changed_by(|a, b| a.id == b.id)
            .materialize();
        let count = Arc::new(AtomicU64::new(0));

        let c = count.clone();
        let _guard = by_id.subscribe(move |_| {
            c.fetch_add(1, Ordering::SeqCst);
        });

        assert_eq!(count.load(Ordering::SeqCst), 1); // initial

        source.set(User {
            id: 1,
            name: "Alicia".into(),
        });
        assert_eq!(count.load(Ordering::SeqCst), 1);

        source.set(User {
            id: 2,
            name: "Bob".into(),
        });
        assert_eq!(count.load(Ordering::SeqCst), 2);

        source.set(User {
            id: 2,
            name: "Robert".into(),
        });
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }
}
