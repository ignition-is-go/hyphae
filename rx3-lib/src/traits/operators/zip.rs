use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use std::collections::VecDeque;
use crate::cell::{Cell, CellImmutable, CellMutable};
use super::{Gettable, Watchable};

pub trait ZipExt<T>: Watchable<T> {
    /// Zip with another cell - pairs values in order.
    /// Unlike join (which uses latest), zip pairs values 1:1.
    fn zip<U, M>(&self, other: &Cell<U, M>) -> Cell<(T, U), CellImmutable>
    where
        T: Clone + Send + Sync + 'static,
        U: Clone + Send + Sync + 'static,
        M: Send + Sync + 'static,
        Self: Clone + Send + Sync + 'static,
    {
        let initial = (self.get(), other.get());
        let derived = Cell::<(T, U), CellMutable>::new(initial);

        // Queues to buffer values
        let left_queue: Arc<Mutex<VecDeque<T>>> = Arc::new(Mutex::new(VecDeque::new()));
        let right_queue: Arc<Mutex<VecDeque<U>>> = Arc::new(Mutex::new(VecDeque::new()));

        // Subscribe to self
        let weak1 = derived.downgrade();
        let first1 = Arc::new(AtomicBool::new(true));
        let lq1 = left_queue.clone();
        let rq1 = right_queue.clone();
        let guard1 = self.subscribe(move |value| {
            if first1.swap(false, Ordering::SeqCst) {
                return;
            }
            let mut rq = rq1.lock().unwrap();
            if let Some(right) = rq.pop_front() {
                drop(rq);
                if let Some(d) = weak1.upgrade() {
                    d.notify((value.clone(), right));
                }
            } else {
                drop(rq);
                lq1.lock().unwrap().push_back(value.clone());
            }
        });
        derived.own(guard1);

        // Subscribe to other
        let weak2 = derived.downgrade();
        let first2 = Arc::new(AtomicBool::new(true));
        let lq2 = left_queue;
        let rq2 = right_queue;
        let guard2 = other.subscribe(move |value| {
            if first2.swap(false, Ordering::SeqCst) {
                return;
            }
            let mut lq = lq2.lock().unwrap();
            if let Some(left) = lq.pop_front() {
                drop(lq);
                if let Some(d) = weak2.upgrade() {
                    d.notify((left, value.clone()));
                }
            } else {
                drop(lq);
                rq2.lock().unwrap().push_back(value.clone());
            }
        });
        derived.own(guard2);

        // Complete when EITHER source completes (no more pairs possible)
        let weak = derived.downgrade();
        let complete_guard1 = self.on_complete(move || {
            if let Some(d) = weak.upgrade() {
                d.complete();
            }
        });
        derived.own(complete_guard1);

        let weak = derived.downgrade();
        let complete_guard2 = other.on_complete(move || {
            if let Some(d) = weak.upgrade() {
                d.complete();
            }
        });
        derived.own(complete_guard2);

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
