use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use crate::cell::{Cell, CellImmutable, CellMutable};
use crate::signal::Signal;
use super::Watchable;

pub trait FilterExt<T>: Watchable<T> {
    fn filter(&self, predicate: impl Fn(&T) -> bool + Send + Sync + 'static) -> Cell<T, CellImmutable>
    where
        T: Clone + Send + Sync + 'static,
        Self: Clone + Send + Sync + 'static,
    {
        let cell = Cell::<T, CellMutable>::new(self.get());
        let cell_id = cell.id();

        let weak = cell.downgrade();
        let predicate = Arc::new(predicate);
        let first = Arc::new(AtomicBool::new(true));
        let guard = self.subscribe(move |signal| {
            if let Some(c) = weak.upgrade() {
                match signal {
                    Signal::Value(value, ctx) => {
                        if first.swap(false, Ordering::SeqCst) {
                            return;
                        }
                        if predicate(value.as_ref()) {
                            // Propagate with updated transaction context
                            let new_ctx = ctx.as_ref().and_then(|c| c.push(cell_id));
                            c.notify(Signal::Value(value.clone(), new_ctx));
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

impl<T, W: Watchable<T>> FilterExt<T> for W {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Mutable;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    #[test]
    fn test_filter_passes_matching() {
        let source = Cell::new(10u64);
        let evens = source.filter(|x| x % 2 == 0);
        let received = Arc::new(AtomicU64::new(0));

        let r = received.clone();
        let _guard = evens.subscribe(move |signal| {
            if let Signal::Value(v, _) = signal {
                r.store(**v, Ordering::SeqCst);
            }
        });

        assert_eq!(received.load(Ordering::SeqCst), 10);

        source.set(4);
        assert_eq!(received.load(Ordering::SeqCst), 4);
    }

    #[test]
    fn test_filter_blocks_non_matching() {
        let source = Cell::new(10u64);
        let evens = source.filter(|x| x % 2 == 0);
        let received = Arc::new(AtomicU64::new(0));

        let r = received.clone();
        let _guard = evens.subscribe(move |signal| {
            if let Signal::Value(v, _) = signal {
                r.store(**v, Ordering::SeqCst);
            }
        });

        source.set(3); // odd - should not pass
        assert_eq!(received.load(Ordering::SeqCst), 10); // still 10

        source.set(6); // even - should pass
        assert_eq!(received.load(Ordering::SeqCst), 6);
    }
}
