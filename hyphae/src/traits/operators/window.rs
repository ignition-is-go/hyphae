use std::collections::VecDeque;

use super::{CellValue, MapExt, ScanExt, Watchable};
use crate::cell::{Cell, CellImmutable};

pub trait WindowExt<T>: Watchable<T> {
    /// Collect values into a sliding window of size `count`.
    ///
    /// Emits a `Vec<T>` containing the most recent `count` values each time
    /// a new value arrives. Before `count` values are collected, emits the
    /// values collected so far.
    ///
    /// # Example
    ///
    /// ```
    /// use hyphae::{Cell, Mutable, Gettable, WindowExt};
    ///
    /// let source = Cell::new(0);
    /// let windowed = source.window(3);
    ///
    /// assert_eq!(windowed.get(), vec![0]);  // Initial value
    ///
    /// source.set(1);
    /// assert_eq!(windowed.get(), vec![0, 1]);  // Growing window
    ///
    /// source.set(2);
    /// assert_eq!(windowed.get(), vec![0, 1, 2]);  // Full window
    ///
    /// source.set(3);
    /// assert_eq!(windowed.get(), vec![1, 2, 3]);  // Sliding window
    /// ```
    #[track_caller]
    fn window(&self, count: usize) -> Cell<Vec<T>, CellImmutable>
    where
        T: CellValue,
        Self: Clone + Send + Sync + 'static,
    {
        assert!(count > 0, "window size must be positive");

        self.scan(VecDeque::with_capacity(count), move |acc, value| {
            let mut new_acc = acc.clone();
            new_acc.push_back(value.clone());
            if new_acc.len() > count {
                new_acc.pop_front();
            }
            new_acc
        })
        .map(|deque| deque.iter().cloned().collect())
    }
}

impl<T, W: Watchable<T>> WindowExt<T> for W {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Gettable, Mutable};

    #[test]
    fn test_window_sliding() {
        let source = Cell::new(0);
        let windowed = source.window(3);

        assert_eq!(windowed.get(), vec![0]); // Initial

        source.set(1);
        assert_eq!(windowed.get(), vec![0, 1]);

        source.set(2);
        assert_eq!(windowed.get(), vec![0, 1, 2]); // Full window

        source.set(3);
        assert_eq!(windowed.get(), vec![1, 2, 3]); // Slides

        source.set(4);
        assert_eq!(windowed.get(), vec![2, 3, 4]); // Slides
    }

    #[test]
    fn test_window_size_one() {
        let source = Cell::new(10);
        let windowed = source.window(1);

        assert_eq!(windowed.get(), vec![10]);

        source.set(20);
        assert_eq!(windowed.get(), vec![20]);

        source.set(30);
        assert_eq!(windowed.get(), vec![30]);
    }
}
