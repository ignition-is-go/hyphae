use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use crossbeam::queue::SegQueue;

use crate::cell::{Cell, CellImmutable, CellMutable};
use crate::signal::Signal;

use super::Watchable;

pub trait WindowExt<T>: Watchable<T> {
    /// Collect values into windows of size `count`.
    ///
    /// Similar to buffer_count but conceptually represents sliding windows.
    /// Each emission is a Vec of the last `count` values.
    ///
    /// # Example
    ///
    /// ```
    /// use rx3::{Cell, Mutable, WindowExt, Watchable};
    ///
    /// let source = Cell::new(0);
    /// let windowed = source.window(3);
    ///
    /// source.set(1);
    /// source.set(2);
    /// source.set(3); // Emits [1, 2, 3]
    /// ```
    fn window(&self, count: usize) -> Cell<Vec<T>, CellImmutable>
    where
        T: Clone + Send + Sync + 'static,
        Self: Clone + Send + Sync + 'static,
    {
        assert!(count > 0, "window size must be positive");

        let derived = Cell::<Vec<T>, CellMutable>::new(Vec::new());

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
                            let mut window = Vec::with_capacity(count);
                            for _ in 0..count {
                                if let Some(v) = buffer.pop() {
                                    window.push(v);
                                }
                            }
                            buffer_len.fetch_sub(count, Ordering::SeqCst);
                            d.notify(Signal::value(window));
                        }
                    }
                    Signal::Complete => {
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

impl<T, W: Watchable<T>> WindowExt<T> for W {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Mutable;
    use std::sync::Mutex;

    #[test]
    fn test_window() {
        let source = Cell::new(0);
        let windowed = source.window(3);

        let emissions = Arc::new(Mutex::new(Vec::<Vec<i32>>::new()));
        let e = emissions.clone();
        let _guard = windowed.subscribe(move |signal| {
            if let Signal::Value(v) = signal {
                e.lock().unwrap().push((**v).clone());
            }
        });

        source.set(1);
        source.set(2);
        source.set(3);
        source.set(4);
        source.set(5);
        source.set(6);

        let emitted = emissions.lock().unwrap();
        assert!(emitted.len() >= 3); // Initial + 2 windows
        assert_eq!(emitted[1], vec![1, 2, 3]);
        assert_eq!(emitted[2], vec![4, 5, 6]);
    }
}
