use std::collections::VecDeque;

use super::{CellValue, MapExt, MapPipeline, ScanExt, ScanPipeline};
use crate::pipeline::{Definite, Pipeline, PipelineSeed};

#[allow(private_bounds)]
pub trait WindowExt<T: CellValue>: Pipeline<T, Definite> + PipelineSeed<T> {
    /// Collect values into a sliding window of size `count`.
    ///
    /// Emits a `Vec<T>` containing the most recent `count` values each time
    /// a new value arrives. Before `count` values are collected, emits the
    /// values collected so far.
    ///
    /// # Example
    ///
    /// ```
    /// use hyphae::{Cell, Mutable, Gettable, MaterializeDefinite, WindowExt};
    ///
    /// let source = Cell::new(0);
    /// let windowed = source.clone().window(3).materialize();
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
    fn window(
        self,
        count: usize,
    ) -> MapPipeline<
        ScanPipeline<
            Self,
            T,
            VecDeque<T>,
            impl Fn(&VecDeque<T>, &T) -> VecDeque<T> + Send + Sync + 'static,
        >,
        VecDeque<T>,
        Vec<T>,
        impl Fn(&VecDeque<T>) -> Vec<T> + Send + Sync + 'static,
    > {
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

impl<T: CellValue, P: Pipeline<T, Definite> + PipelineSeed<T>> WindowExt<T> for P {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Cell, Gettable, MaterializeDefinite, Mutable};

    #[test]
    fn window_installs_only_when_materialized() {
        let source = Cell::new(0);
        let initial_subscribers = crate::traits::DepNode::subscriber_count(&source);
        let pipeline = source.clone().window(3);

        assert_eq!(
            crate::traits::DepNode::subscriber_count(&source),
            initial_subscribers
        );

        let _windowed = pipeline.materialize();
        assert_eq!(
            crate::traits::DepNode::subscriber_count(&source),
            initial_subscribers + 1
        );
    }

    #[test]
    fn test_window_sliding() {
        let source = Cell::new(0);
        let windowed = source.clone().window(3).materialize();

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
        let windowed = source.clone().window(1).materialize();

        assert_eq!(windowed.get(), vec![10]);

        source.set(20);
        assert_eq!(windowed.get(), vec![20]);

        source.set(30);
        assert_eq!(windowed.get(), vec![30]);
    }
}
