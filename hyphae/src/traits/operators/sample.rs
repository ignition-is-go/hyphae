//! `sample(notifier)` operator — emit the source's current value whenever the
//! notifier fires.
//!
//! Returns a [`Pipeline`] (specifically a [`MapPipeline`] over the notifier).
//! Materialize to subscribe; further operators can also chain off it.

use super::{super::operators::MapExt, CellValue, Gettable, MapPipeline, Watchable};
use crate::pipeline::{Pipeline, PipelineSeed};

pub trait SampleExt<T>: Watchable<T> {
    /// Emit the source's latest value each time the notifier fires.
    ///
    /// Ignores the notifier's value; only uses it as a trigger.
    ///
    /// # Example
    ///
    /// ```
    /// use hyphae::{Cell, Gettable, MaterializeDefinite, Mutable, SampleExt};
    ///
    /// let source = Cell::new(0);
    /// let ticker = Cell::new(());
    /// let sampled = source.sample(&ticker).materialize();
    ///
    /// source.set(1);
    /// source.set(2);
    /// source.set(3);
    /// ticker.set(()); // emits 3 (latest from source)
    ///
    /// source.set(4);
    /// ticker.set(()); // emits 4
    /// ```
    #[track_caller]
    #[allow(private_bounds)]
    fn sample<N, U>(
        &self,
        notifier: &N,
    ) -> MapPipeline<N, U, T, impl Fn(&U) -> T + Send + Sync + 'static>
    where
        T: CellValue,
        U: CellValue,
        N: Pipeline<U> + PipelineSeed<U> + Clone + Send + Sync + 'static,
        Self: Gettable<T> + Clone + Send + Sync + 'static,
    {
        let source = self.clone();
        notifier.clone().map(move |_| source.get())
    }
}

impl<T, W: Watchable<T>> SampleExt<T> for W {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Cell, MaterializeDefinite, Mutable, Signal,
        cell::CellImmutable,
    };

    #[test]
    fn test_sample() {
        let source = Cell::new(0);
        let ticker = Cell::new(());
        let sampled: Cell<i32, CellImmutable> = source.sample(&ticker).materialize();

        let (tx, rx) = std::sync::mpsc::channel::<i32>();
        let _guard = sampled.subscribe(move |signal| {
            if let Signal::Value(v) = signal {
                let _ = tx.send(**v);
            }
        });

        // Initial value
        assert_eq!(rx.recv().ok(), Some(0));

        // Source changes but no emission yet
        source.set(1);
        source.set(2);
        source.set(3);
        assert!(rx.try_recv().is_err());

        // Ticker fires — emits latest (3)
        ticker.set(());
        assert_eq!(rx.recv().ok(), Some(3));

        // More source changes
        source.set(4);
        source.set(5);

        // Ticker fires — emits 5
        ticker.set(());
        assert_eq!(rx.recv().ok(), Some(5));
    }
}
