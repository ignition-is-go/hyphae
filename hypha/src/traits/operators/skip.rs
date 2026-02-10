use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use super::{CellValue, Watchable};
use crate::{
    cell::{Cell, CellImmutable, CellMutable},
    signal::Signal,
};

pub trait SkipExt<T>: Watchable<T> {
    #[track_caller]
    fn skip(&self, count: usize) -> Cell<T, CellImmutable>
    where
        T: CellValue,
        Self: Clone + Send + Sync + 'static,
    {
        let cell = Cell::<T, CellMutable>::new(self.get());
        let cell = if let Some(name) = self.name() {
            cell.with_name(format!("{}::skip", name))
        } else {
            cell
        };

        let to_skip = Arc::new(AtomicUsize::new(count));
        let weak = cell.downgrade();
        let guard = self.subscribe(move |signal| {
            if let Some(c) = weak.upgrade() {
                match signal {
                    Signal::Value(_) => {
                        let prev = to_skip.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                            if n > 0 { Some(n - 1) } else { None }
                        });
                        if prev.is_err() {
                            c.notify(signal.clone()); // Arc clone, no deep copy
                        }
                    }
                    Signal::Complete => c.notify(Signal::Complete),
                    Signal::Error(e) => c.notify(Signal::Error(e.clone())),
                }
            }
        });
        cell.own(guard);

        cell.lock()
    }
}

impl<T, W: Watchable<T>> SkipExt<T> for W {}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicU64, Ordering as AtomicOrdering},
    };

    use super::*;
    use crate::Mutable;

    #[test]
    fn test_skip_ignores_first_n() {
        let source = Cell::new(0u64);
        let skipped = source.skip(2);
        let count = Arc::new(AtomicU64::new(0));

        let c = count.clone();
        let _guard = skipped.subscribe(move |_| {
            c.fetch_add(1, AtomicOrdering::SeqCst);
        });

        // Watch always fires once immediately with current value
        assert_eq!(count.load(AtomicOrdering::SeqCst), 1);

        // First set - skipped (to_skip goes 2 -> 1 from initial watch, then 1 -> 0 here)
        source.set(1);
        assert_eq!(count.load(AtomicOrdering::SeqCst), 1);

        // Second set - now emits (to_skip is 0)
        source.set(2);
        assert_eq!(count.load(AtomicOrdering::SeqCst), 2);

        // Third set - emits
        source.set(3);
        assert_eq!(count.load(AtomicOrdering::SeqCst), 3);
    }
}
