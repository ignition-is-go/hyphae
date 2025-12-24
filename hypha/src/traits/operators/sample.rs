use super::{super::operators::MapExt, Gettable, Watchable};
use crate::cell::{Cell, CellImmutable};

pub trait SampleExt<T>: Watchable<T> {
    /// Sample the source whenever the notifier emits.
    ///
    /// Emits the latest value from the source each time the notifier fires.
    /// Ignores the notifier's value; only uses it as a trigger.
    ///
    /// # Example
    ///
    /// ```
    /// use hypha::{Cell, Mutable, Gettable, SampleExt};
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
    fn sample<N, U>(&self, notifier: &N) -> Cell<T, CellImmutable>
    where
        T: Clone + Send + Sync + 'static,
        U: Clone + Send + Sync + 'static,
        N: Watchable<U> + Clone + Send + Sync + 'static,
        Self: Gettable<T> + Clone + Send + Sync + 'static,
    {
        // When notifier fires, get the current value from source
        let source = self.clone();
        notifier.map(move |_| source.get())
    }
}

impl<T, W: Watchable<T>> SampleExt<T> for W {}

#[cfg(test)]
#[allow(clippy::disallowed_types)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::{Mutable, Signal};

    #[test]
    fn test_sample() {
        let source = Cell::new(0);
        let ticker = Cell::new(());
        let sampled = source.sample(&ticker);

        let emissions = Arc::new(Mutex::new(Vec::new()));
        let e = emissions.clone();
        let _guard = sampled.subscribe(move |signal| {
            if let Signal::Value(v) = signal {
                e.lock().unwrap().push(**v);
            }
        });

        // Initial value
        assert_eq!(emissions.lock().unwrap().clone(), vec![0]);

        // Source changes but no emission yet
        source.set(1);
        source.set(2);
        source.set(3);
        assert_eq!(emissions.lock().unwrap().clone(), vec![0]);

        // Ticker fires - emits latest (3)
        ticker.set(());
        assert_eq!(emissions.lock().unwrap().clone(), vec![0, 3]);

        // More source changes
        source.set(4);
        source.set(5);

        // Ticker fires - emits 5
        ticker.set(());
        assert_eq!(emissions.lock().unwrap().clone(), vec![0, 3, 5]);
    }
}
