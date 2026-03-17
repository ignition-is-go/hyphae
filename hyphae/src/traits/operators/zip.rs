use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use crossbeam::queue::SegQueue;

use super::{CellValue, Gettable, Watchable};
use crate::{
    cell::{Cell, CellImmutable, CellMutable},
    signal::Signal,
};

pub trait ZipExt<T>: Watchable<T> {
    /// Zip with another cell - pairs values in order.
    /// Unlike join (which uses latest), zip pairs values 1:1.
    #[track_caller]
    fn zip<U, M>(&self, other: &Cell<U, M>) -> Cell<(T, U), CellImmutable>
    where
        T: CellValue,
        U: CellValue,
        M: Send + Sync + 'static,
        Self: Clone + Send + Sync + 'static,
    {
        let initial = (self.get(), other.get());
        let derived = Cell::<(T, U), CellMutable>::new(initial);
        let derived = if let Some(name) = self.name() {
            derived.with_name(format!("{}::zip", name))
        } else {
            derived
        };

        // Lock-free queues to buffer values
        let left_queue: Arc<SegQueue<T>> = Arc::new(SegQueue::new());
        let right_queue: Arc<SegQueue<U>> = Arc::new(SegQueue::new());

        // Subscribe to self
        let weak1 = derived.downgrade();
        let first1 = Arc::new(AtomicBool::new(true));
        let lq1 = left_queue.clone();
        let rq1 = right_queue.clone();
        let guard1 = self.subscribe(move |signal| {
            if let Some(d) = weak1.upgrade() {
                match signal {
                    Signal::Value(value) => {
                        if first1.swap(false, Ordering::SeqCst) {
                            return;
                        }
                        if let Some(right) = rq1.pop() {
                            d.notify(Signal::value((value.as_ref().clone(), right)));
                        } else {
                            lq1.push(value.as_ref().clone());
                        }
                    }
                    // Complete when EITHER source completes (no more pairs possible)
                    Signal::Complete => d.notify(Signal::Complete),
                    Signal::Error(e) => d.notify(Signal::Error(e.clone())),
                }
            }
        });
        derived.own(guard1);

        // Subscribe to other
        let weak2 = derived.downgrade();
        let first2 = Arc::new(AtomicBool::new(true));
        let lq2 = left_queue;
        let rq2 = right_queue;
        let guard2 = other.subscribe(move |signal| {
            if let Some(d) = weak2.upgrade() {
                match signal {
                    Signal::Value(value) => {
                        if first2.swap(false, Ordering::SeqCst) {
                            return;
                        }
                        if let Some(left) = lq2.pop() {
                            d.notify(Signal::value((left, value.as_ref().clone())));
                        } else {
                            rq2.push(value.as_ref().clone());
                        }
                    }
                    Signal::Complete => d.notify(Signal::Complete),
                    Signal::Error(e) => d.notify(Signal::Error(e.clone())),
                }
            }
        });
        derived.own(guard2);

        derived.lock()
    }
}

impl<T, W: Watchable<T>> ZipExt<T> for W {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Gettable, Mutable};

    #[test]
    fn test_zip() {
        let a = Cell::new(1);
        let b = Cell::new("a");
        let zipped = a.zip(&b);

        assert_eq!(zipped.get(), (1, "a")); // Initial from both

        a.set(2);
        assert_eq!(zipped.get(), (1, "a")); // No match yet - 2 queued

        a.set(3);
        assert_eq!(zipped.get(), (1, "a")); // No match yet - 3 queued

        b.set("b");
        assert_eq!(zipped.get(), (2, "b")); // Pairs 2 with "b"

        b.set("c");
        assert_eq!(zipped.get(), (3, "c")); // Pairs 3 with "c"
    }
}
