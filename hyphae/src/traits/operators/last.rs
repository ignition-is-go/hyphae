//! `last` operator — emit only the most recent value, when the source completes.
//!
//! [`Empty`] seedness: nothing lands in the cell until the source emits
//! `Complete`. After completion, the cell holds `Some(last_value)`. If the
//! source never emits any values before completing, the cell stays `None`
//! (use [`LastExt::last_or`] to pick a default in that case).

use std::{
    marker::PhantomData,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering as AtomicOrdering},
    },
};

use arc_swap::ArcSwap;

use super::CellValue;
use crate::{
    pipeline::{Empty, MaterializeEmpty, Pipeline, PipelineInstall, Seedness},
    signal::Signal,
    subscription::SubscriptionGuard,
};

/// Pipeline node representing `source.last()`.
pub struct LastPipeline<S, T, Sd = crate::pipeline::Definite> {
    source: S,
    _t: PhantomData<fn(T)>,
    _sd: PhantomData<fn(Sd)>,
}

impl<S, T, Sd> PipelineInstall<T> for LastPipeline<S, T, Sd>
where
    S: PipelineInstall<T> + Send + Sync + 'static,
    Sd: Seedness,
    T: CellValue,
{
    fn install(
        &self,
        callback: Arc<dyn Fn(&Signal<T>) + Send + Sync>,
    ) -> SubscriptionGuard {
        // Skip the synchronous-on-subscribe initial replay so the source's
        // retained value does not count as an emission; only true post-subscribe
        // emissions feed `last_value`.
        let last_value: Arc<ArcSwap<Option<T>>> = Arc::new(ArcSwap::from_pointee(None));
        let first = Arc::new(AtomicBool::new(true));
        let wrapped: Arc<dyn Fn(&Signal<T>) + Send + Sync> = Arc::new(move |signal: &Signal<T>| {
            match signal {
                Signal::Value(v) => {
                    if first.swap(false, AtomicOrdering::SeqCst) {
                        return;
                    }
                    last_value.store(Arc::new(Some(v.as_ref().clone())));
                }
                Signal::Complete => {
                    let val = last_value.load();
                    if let Some(v) = (**val).clone() {
                        callback(&Signal::value(v));
                    }
                    callback(&Signal::Complete);
                }
                Signal::Error(e) => callback(&Signal::Error(e.clone())),
            }
        });
        self.source.install(wrapped)
    }
}

#[allow(private_bounds)]
impl<S, T, Sd> Pipeline<T, Empty> for LastPipeline<S, T, Sd>
where
    S: Pipeline<T, Sd>,
    Sd: Seedness,
    T: CellValue,
{
}

impl<S, T, Sd> MaterializeEmpty<T> for LastPipeline<S, T, Sd>
where
    S: Pipeline<T, Sd>,
    Sd: Seedness,
    T: CellValue,
{
}

/// Pipeline node representing `source.last_or(default)`.
pub struct LastOrPipeline<S, T, Sd = crate::pipeline::Definite> {
    source: S,
    default: T,
    _sd: PhantomData<fn(Sd)>,
}

impl<S, T, Sd> PipelineInstall<T> for LastOrPipeline<S, T, Sd>
where
    S: PipelineInstall<T> + Send + Sync + 'static,
    Sd: Seedness,
    T: CellValue,
{
    fn install(
        &self,
        callback: Arc<dyn Fn(&Signal<T>) + Send + Sync>,
    ) -> SubscriptionGuard {
        let last_value: Arc<ArcSwap<Option<T>>> = Arc::new(ArcSwap::from_pointee(None));
        let default = self.default.clone();
        let first = Arc::new(AtomicBool::new(true));
        let wrapped: Arc<dyn Fn(&Signal<T>) + Send + Sync> = Arc::new(move |signal: &Signal<T>| {
            match signal {
                Signal::Value(v) => {
                    if first.swap(false, AtomicOrdering::SeqCst) {
                        return;
                    }
                    last_value.store(Arc::new(Some(v.as_ref().clone())));
                }
                Signal::Complete => {
                    let val = last_value.load();
                    let emit = match &**val {
                        Some(v) => v.clone(),
                        None => default.clone(),
                    };
                    callback(&Signal::value(emit));
                    callback(&Signal::Complete);
                }
                Signal::Error(e) => callback(&Signal::Error(e.clone())),
            }
        });
        self.source.install(wrapped)
    }
}

#[allow(private_bounds)]
impl<S, T, Sd> Pipeline<T, Empty> for LastOrPipeline<S, T, Sd>
where
    S: Pipeline<T, Sd>,
    Sd: Seedness,
    T: CellValue,
{
}

impl<S, T, Sd> MaterializeEmpty<T> for LastOrPipeline<S, T, Sd>
where
    S: Pipeline<T, Sd>,
    Sd: Seedness,
    T: CellValue,
{
}

#[allow(private_bounds)]
pub trait LastExt<T: CellValue, S: Seedness>: Pipeline<T, S> {
    /// Emit only the most recent value when the source completes.
    ///
    /// # Example
    ///
    /// ```
    /// use hyphae::{Cell, Gettable, LastExt, MaterializeEmpty, Mutable};
    ///
    /// let source = Cell::new(0);
    /// let last = source.clone().last().materialize();
    ///
    /// source.set(1);
    /// source.set(2);
    /// source.set(3);
    /// source.complete(); // emits 3
    ///
    /// assert_eq!(last.get(), Some(3));
    /// ```
    #[track_caller]
    fn last(self) -> LastPipeline<Self, T, S> {
        LastPipeline {
            source: self,
            _t: PhantomData,
            _sd: PhantomData,
        }
    }

    /// Emit only the most recent value, or `default` if the source completes
    /// without emitting any values.
    ///
    /// # Example
    ///
    /// ```
    /// use hyphae::{Cell, Gettable, LastExt, MaterializeEmpty, Mutable};
    ///
    /// let source = Cell::new(0);
    /// let last = source.clone().last_or(999).materialize();
    ///
    /// source.complete(); // no values set, emits 999
    ///
    /// assert_eq!(last.get(), Some(999));
    /// ```
    #[track_caller]
    fn last_or(self, default: T) -> LastOrPipeline<Self, T, S> {
        LastOrPipeline {
            source: self,
            default,
            _sd: PhantomData,
        }
    }
}

impl<T: CellValue, S: Seedness, P: Pipeline<T, S>> LastExt<T, S> for P {}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;
    use crate::{Cell, Gettable, MaterializeEmpty, Mutable, traits::Watchable};

    #[test]
    fn test_last() {
        let source = Cell::new(0);
        let last = source.clone().last().materialize();

        let value_emissions = Arc::new(AtomicU32::new(0));
        let ve = value_emissions.clone();
        let _guard = last.subscribe(move |signal| {
            if let Signal::Value(_) = signal {
                ve.fetch_add(1, Ordering::SeqCst);
            }
        });

        // Initial sync emit fires once with the cell's None value.
        assert_eq!(value_emissions.load(Ordering::SeqCst), 1);

        source.set(1);
        source.set(2);
        source.set(3);
        // Still no Some emission — last() only emits on Complete.
        assert_eq!(value_emissions.load(Ordering::SeqCst), 1);

        source.complete();
        // Now emits the last value, lifted to Some.
        assert_eq!(value_emissions.load(Ordering::SeqCst), 2);
        assert_eq!(last.get(), Some(3));
    }

    #[test]
    fn test_last_or_with_values() {
        let source = Cell::new(0);
        let last = source.clone().last_or(999).materialize();

        source.set(1);
        source.set(2);
        source.complete();

        assert_eq!(last.get(), Some(2));
    }

    #[test]
    fn test_last_or_without_values() {
        let source = Cell::new(0);
        let last = source.clone().last_or(999).materialize();

        // No set() calls, just complete.
        source.complete();

        assert_eq!(last.get(), Some(999));
    }
}
