//! `with_latest_from(other)` operator — pair every emission with the latest
//! value of another `Gettable` cell.
//!
//! Returns a [`Pipeline`] (specifically a [`MapPipeline`] over self).
//! Materialize to subscribe; further operators can also chain off it.

use super::{super::operators::MapExt, CellValue, Gettable, MapPipeline};
use crate::pipeline::{Pipeline, PipelineSeed};

#[allow(private_bounds)]
pub trait WithLatestFromExt<T: CellValue>: Pipeline<T> + PipelineSeed<T> {
    /// On each `self` emission, pair the value with the latest from `other`.
    ///
    /// Unlike `join()` (which emits when either source changes), this only
    /// emits when the primary source changes; `other.get()` is read at
    /// emission time.
    ///
    /// # Example
    ///
    /// ```
    /// use hyphae::{Cell, Gettable, MaterializeDefinite, Mutable, WithLatestFromExt};
    ///
    /// let clicks = Cell::new(0u32);
    /// let mouse_pos = Cell::new((0, 0));
    ///
    /// let click_positions = clicks.with_latest_from(&mouse_pos).materialize();
    ///
    /// mouse_pos.set((10, 20)); // no emission
    /// mouse_pos.set((30, 40)); // no emission
    /// clicks.set(1);           // emits (1, (30, 40))
    /// ```
    #[track_caller]
    fn with_latest_from<U, W2>(
        &self,
        other: &W2,
    ) -> MapPipeline<Self, T, (T, U), impl Fn(&T) -> (T, U) + Send + Sync + 'static>
    where
        T: CellValue,
        U: CellValue,
        W2: Gettable<U> + Clone + Send + Sync + 'static,
        Self: Clone + Send + Sync + 'static,
    {
        let other = other.clone();
        self.clone().map(move |t| (t.clone(), other.get()))
    }
}

impl<T: CellValue, P: Pipeline<T> + PipelineSeed<T>> WithLatestFromExt<T> for P {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Cell, MaterializeDefinite, Mutable, Signal, cell::CellImmutable, traits::Watchable,
    };

    #[test]
    fn test_with_latest_from() {
        let source = Cell::new(0);
        let other = Cell::new("a".to_string());
        let combined: Cell<(i32, String), CellImmutable> =
            source.with_latest_from(&other).materialize();

        let (tx, rx) = std::sync::mpsc::channel::<(i32, String)>();
        let _guard = combined.subscribe(move |signal| {
            if let Signal::Value(v) = signal {
                let _ = tx.send((**v).clone());
            }
        });

        // Initial combined value
        assert_eq!(rx.recv().ok(), Some((0, "a".to_string())));

        // Other changes - no emission
        other.set("b".to_string());
        other.set("c".to_string());
        assert!(rx.try_recv().is_err());

        // Source changes - emits with latest other
        source.set(1);
        assert_eq!(rx.recv().ok(), Some((1, "c".to_string())));

        // Other changes again - no emission
        other.set("d".to_string());
        assert!(rx.try_recv().is_err());

        // Source changes - emits with latest other
        source.set(2);
        assert_eq!(rx.recv().ok(), Some((2, "d".to_string())));
    }
}
