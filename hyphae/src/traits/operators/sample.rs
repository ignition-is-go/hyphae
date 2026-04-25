use super::{super::operators::MapExt, CellValue, Gettable, Watchable};
use crate::{
    cell::{Cell, CellImmutable},
    pipeline::Pipeline,
};

pub trait SampleExt<T>: Watchable<T> {
    /// Sample the source whenever the notifier emits.
    ///
    /// Emits the latest value from the source each time the notifier fires.
    /// Ignores the notifier's value; only uses it as a trigger.
    ///
    /// # Example
    ///
    /// ```
    /// use hyphae::{Cell, Mutable, Gettable, SampleExt};
    ///
    /// let source = Cell::new(0);
    /// let ticker = Cell::new(());
    /// let sampled = source.sample(&ticker);
    ///
    /// source.set(1);
    /// source.set(2);
    /// source.set(3);
    /// ticker.set(()); // Emits 3 (latest from source)
    ///
    /// source.set(4);
    /// ticker.set(()); // Emits 4
    /// ```
    #[track_caller]
    fn sample<N, U>(&self, notifier: &N) -> Cell<T, CellImmutable>
    where
        T: CellValue,
        U: CellValue,
        N: Watchable<U> + Clone + Send + Sync + 'static,
        Self: Gettable<T> + Clone + Send + Sync + 'static,
    {
        // When notifier fires, get the current value from source
        let source = self.clone();
        notifier.map(move |_| source.get()).materialize()
    }
}

impl<T, W: Watchable<T>> SampleExt<T> for W {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Mutable, Signal};

    #[test]
    fn test_sample() {
        let source = Cell::new(0);
        let ticker = Cell::new(());
        let sampled = source.sample(&ticker);

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

        // Ticker fires - emits latest (3)
        ticker.set(());
        assert_eq!(rx.recv().ok(), Some(3));

        // More source changes
        source.set(4);
        source.set(5);

        // Ticker fires - emits 5
        ticker.set(());
        assert_eq!(rx.recv().ok(), Some(5));
    }
}
