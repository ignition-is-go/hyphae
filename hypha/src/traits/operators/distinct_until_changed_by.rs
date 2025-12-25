use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use arc_swap::ArcSwap;

use crate::cell::{Cell, CellImmutable, CellMutable};
use crate::signal::Signal;

use super::Watchable;

pub trait DistinctUntilChangedByExt<T>: Watchable<T> {
    /// Like `deduped()` but with a custom comparator.
    ///
    /// Only emits when the comparator returns `false` (values are different).
    ///
    /// # Example
    ///
    /// ```
    /// use hypha::{Cell, Mutable, Gettable, DistinctUntilChangedByExt};
    ///
    /// #[derive(Clone)]
    /// struct User { id: u32, name: String }
    ///
    /// let source = Cell::new(User { id: 1, name: "Alice".into() });
    /// let by_id = source.distinct_until_changed_by(|a, b| a.id == b.id);
    ///
    /// source.set(User { id: 1, name: "Alicia".into() }); // Same id - blocked
    /// source.set(User { id: 2, name: "Bob".into() });    // Different id - passes
    /// ```
    fn distinct_until_changed_by<F>(&self, comparator: F) -> Cell<T, CellImmutable>
    where
        T: Clone + Send + Sync + 'static,
        F: Fn(&T, &T) -> bool + Send + Sync + 'static,
        Self: Clone + Send + Sync + 'static,
    {
        let derived = Cell::<T, CellMutable>::new(self.get());

        let weak = derived.downgrade();
        let first = Arc::new(AtomicBool::new(true));
        let last_value: Arc<ArcSwap<T>> = Arc::new(ArcSwap::from_pointee(self.get()));
        let comparator = Arc::new(comparator);

        let guard = self.subscribe(move |signal| {
            if let Some(d) = weak.upgrade() {
                match signal {
                    Signal::Value(value, _) => {
                        if first.swap(false, Ordering::SeqCst) {
                            return;
                        }
                        let last = last_value.load();
                        if !comparator(&**value, &*last) {
                            last_value.store(value.clone());
                            d.notify(signal.clone());
                        }
                    }
                    Signal::Complete => d.notify(Signal::Complete),
                    Signal::Error(e) => d.notify(Signal::Error(e.clone())),
                }
            }
        });
        derived.own(guard);

        derived.lock()
    }
}

impl<T, W: Watchable<T>> DistinctUntilChangedByExt<T> for W {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Mutable;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[derive(Clone)]
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
        let by_id = source.distinct_until_changed_by(|a, b| a.id == b.id);
        let count = Arc::new(AtomicU64::new(0));

        let c = count.clone();
        let _guard = by_id.subscribe(move |_| {
            c.fetch_add(1, Ordering::SeqCst);
        });

        assert_eq!(count.load(Ordering::SeqCst), 1); // initial

        // Same id, different name - blocked
        source.set(User {
            id: 1,
            name: "Alicia".into(),
        });
        assert_eq!(count.load(Ordering::SeqCst), 1);

        // Different id - passes
        source.set(User {
            id: 2,
            name: "Bob".into(),
        });
        assert_eq!(count.load(Ordering::SeqCst), 2);

        // Same id again - blocked
        source.set(User {
            id: 2,
            name: "Robert".into(),
        });
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }
}
