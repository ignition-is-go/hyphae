use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use crossbeam::queue::SegQueue;

use super::{CellValue, Watchable};
use crate::{
    cell::{Cell, CellImmutable, CellMutable},
    signal::Signal,
};

pub trait BufferCountExt<T>: Watchable<T> {
    /// Collect values into non-overlapping chunks of size `count`.
    ///
    /// Emits a `Vec<T>` containing exactly `count` elements each time.
    /// On completion, emits any remaining buffered values (may be less than `count`).
    ///
    /// # Example
    ///
    /// ```
    /// use hypha::{Cell, Mutable, Gettable, BufferCountExt};
    ///
    /// let source = Cell::new(0);
    /// let buffered = source.buffer_count(3);
    ///
    /// source.set(1);
    /// source.set(2);
    /// source.set(3); // Emits [1, 2, 3]
    /// source.set(4);
    /// source.set(5);
    /// source.set(6); // Emits [4, 5, 6]
    /// ```
    #[track_caller]
    fn buffer_count(&self, count: usize) -> Cell<Vec<T>, CellImmutable>
    where
        T: CellValue,
        Self: Clone + Send + Sync + 'static,
    {
        assert!(count > 0, "buffer_count must be positive");

        let derived = Cell::<Vec<T>, CellMutable>::new(Vec::new());
        let derived = if let Some(name) = self.name() {
            derived.with_name(format!("{}::buffer_count", name))
        } else {
            derived
        };

        let weak = derived.downgrade();
        let buffer: Arc<SegQueue<T>> = Arc::new(SegQueue::new());
        let buffer_len = Arc::new(AtomicUsize::new(0));
        let first = Arc::new(AtomicBool::new(true));

        let guard = self.subscribe(move |signal| {
            if let Some(d) = weak.upgrade() {
                match signal {
                    Signal::Value(value) => {
                        if first.swap(false, Ordering::SeqCst) {
                            return;
                        }
                        buffer.push((**value).clone());
                        let len = buffer_len.fetch_add(1, Ordering::SeqCst) + 1;
                        if len >= count {
                            // Drain count items into a vec
                            let mut chunk = Vec::with_capacity(count);
                            for _ in 0..count {
                                if let Some(v) = buffer.pop() {
                                    chunk.push(v);
                                }
                            }
                            buffer_len.fetch_sub(count, Ordering::SeqCst);
                            d.notify(Signal::value(chunk));
                        }
                    }
                    Signal::Complete => {
                        // Emit remaining buffer on complete
                        let mut remainder = Vec::new();
                        while let Some(v) = buffer.pop() {
                            remainder.push(v);
                        }
                        if !remainder.is_empty() {
                            d.notify(Signal::value(remainder));
                        }
                        d.notify(Signal::Complete);
                    }
                    Signal::Error(e) => d.notify(Signal::Error(e.clone())),
                }
            }
        });
        derived.own(guard);

        derived.lock()
    }
}

impl<T, W: Watchable<T>> BufferCountExt<T> for W {}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use super::*;
    use crate::Mutable;

    #[test]
    fn test_buffer_count() {
        let source = Cell::new(0);
        let buffered = source.buffer_count(3);
        let (tx, rx) = std::sync::mpsc::channel::<Vec<i32>>();

        let _guard = buffered.subscribe(move |signal| {
            if let Signal::Value(v) = signal {
                let _ = tx.send((**v).clone());
            }
        });

        // Initial empty vec
        assert_eq!(rx.recv().ok(), Some(vec![]));

        source.set(1);
        source.set(2);
        assert!(rx.try_recv().is_err()); // Not yet

        source.set(3);
        assert_eq!(rx.recv().ok(), Some(vec![1, 2, 3]));

        source.set(4);
        source.set(5);
        source.set(6);
        assert_eq!(rx.recv().ok(), Some(vec![4, 5, 6]));
    }

    #[test]
    fn test_buffer_count_emits_remainder_on_complete() {
        let source = Cell::new(0);
        let buffered = source.buffer_count(3);
        let (tx, rx) = std::sync::mpsc::channel::<Vec<i32>>();
        let completed = Arc::new(AtomicUsize::new(0));

        let c = completed.clone();
        let _guard = buffered.subscribe(move |signal| match signal {
            Signal::Value(v) => {
                let _ = tx.send((**v).clone());
            }
            Signal::Complete => {
                c.fetch_add(1, Ordering::SeqCst);
            }
            _ => {}
        });

        source.set(1);
        source.set(2);
        // Only 2 values, not a full buffer
        assert_eq!(rx.recv().ok(), Some(vec![])); // Just initial

        source.complete();
        // Should emit remainder [1, 2] then complete
        assert_eq!(rx.recv().ok(), Some(vec![1, 2]));
        assert_eq!(completed.load(Ordering::SeqCst), 1);
    }
}
