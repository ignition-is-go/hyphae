use std::{
    collections::VecDeque,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use parking_lot::Mutex;

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

        // Both side-buffers under ONE lock. Zip pairs by arrival index: each
        // sink checks the *opposite* buffer and, on a miss, buffers its own
        // side. Under the `scheduler` feature's wave-parallel draining the two
        // input cells are distinct same-height ids that can notify at the literal
        // same instant on different threads; two independent lock-free queues let
        // both sinks observe the other empty in the same window and buffer both
        // sides, which permanently mis-pairs the two streams from that point on
        // (confirmed by repro). Holding one lock across the whole check-opposite/
        // pop-or-push transition — and across the `notify`, so push order == lock
        // order == emit order, matching `join` — makes the pairing linearizable:
        // the two sinks can no longer each observe the other empty.
        let buffers: Arc<Mutex<(VecDeque<T>, VecDeque<U>)>> =
            Arc::new(Mutex::new((VecDeque::new(), VecDeque::new())));

        // Subscribe to self
        let weak1 = derived.downgrade();
        let first1 = Arc::new(AtomicBool::new(true));
        let buffers1 = buffers.clone();
        let guard1 = self.subscribe(move |signal| {
            if let Some(d) = weak1.upgrade() {
                match signal {
                    Signal::Value(value) => {
                        if first1.swap(false, Ordering::SeqCst) {
                            return;
                        }
                        let mut bufs = buffers1.lock();
                        if let Some(right) = bufs.1.pop_front() {
                            d.notify(Signal::value((value.as_ref().clone(), right)));
                        } else {
                            bufs.0.push_back(value.as_ref().clone());
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
        let buffers2 = buffers;
        let guard2 = other.subscribe(move |signal| {
            if let Some(d) = weak2.upgrade() {
                match signal {
                    Signal::Value(value) => {
                        if first2.swap(false, Ordering::SeqCst) {
                            return;
                        }
                        let mut bufs = buffers2.lock();
                        if let Some(left) = bufs.0.pop_front() {
                            d.notify(Signal::value((left, value.as_ref().clone())));
                        } else {
                            bufs.1.push_back(value.as_ref().clone());
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
