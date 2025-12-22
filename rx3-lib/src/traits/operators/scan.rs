use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use arc_swap::ArcSwap;
use crate::cell::{Cell, CellImmutable, CellMutable};
use crate::signal::Signal;
use super::Watchable;

pub trait ScanExt<T>: Watchable<T> {
    fn scan<U, F>(&self, initial: U, f: F) -> Cell<U, CellImmutable>
    where
        T: Clone + Send + Sync + 'static,
        U: Clone + Send + Sync + 'static,
        F: Fn(&U, &T) -> U + Send + Sync + 'static,
        Self: Clone + Send + Sync + 'static,
    {
        let first_acc = f(&initial, &self.get());
        let cell = Cell::<U, CellMutable>::new(first_acc.clone());

        let acc = Arc::new(ArcSwap::from_pointee(first_acc));
        let weak = cell.downgrade();
        let first = Arc::new(AtomicBool::new(true));
        let guard = self.subscribe(move |signal| {
            if let Some(c) = weak.upgrade() {
                match signal {
                    Signal::Value(value) => {
                        if first.swap(false, Ordering::SeqCst) {
                            return;
                        }
                        let current = (**acc.load()).clone();
                        let next = f(&current, value.as_ref());
                        acc.store(Arc::new(next.clone()));
                        c.notify(Signal::value(next));
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

impl<T, W: Watchable<T>> ScanExt<T> for W {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Gettable, Mutable};

    #[test]
    fn test_scan_accumulates() {
        let source = Cell::new(1u64);
        let sum = source.scan(0u64, |acc, x| acc + x);

        // Initial: 0 + 1 = 1
        assert_eq!(sum.get(), 1);

        source.set(2);
        assert_eq!(sum.get(), 3); // 1 + 2

        source.set(3);
        assert_eq!(sum.get(), 6); // 3 + 3
    }

    #[test]
    fn test_scan_with_different_types() {
        let source = Cell::new(1);
        let collected = source.scan(String::new(), |acc, x| format!("{}{}", acc, x));

        assert_eq!(collected.get(), "1");

        source.set(2);
        assert_eq!(collected.get(), "12");

        source.set(3);
        assert_eq!(collected.get(), "123");
    }
}
