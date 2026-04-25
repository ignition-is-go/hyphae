use super::{super::operators::MapExt, CellValue, Gettable, Watchable};
use crate::{
    cell::{Cell, CellImmutable},
    pipeline::Pipeline,
};

pub trait WithLatestFromExt<T>: Watchable<T> {
    /// When source emits, pair with the latest value from another cell.
    ///
    /// Unlike `join()` which emits when either source changes, this only
    /// emits when the primary source changes, using the latest value from
    /// the secondary source.
    ///
    /// # Example
    ///
    /// ```
    /// use hyphae::{Cell, Mutable, Gettable, WithLatestFromExt};
    ///
    /// let clicks = Cell::new(0u32);
    /// let mouse_pos = Cell::new((0, 0));
    ///
    /// // Only emit on clicks, include current mouse position
    /// let click_positions = clicks.with_latest_from(&mouse_pos);
    ///
    /// mouse_pos.set((10, 20)); // No emission
    /// mouse_pos.set((30, 40)); // No emission
    /// clicks.set(1);           // Emits (1, (30, 40))
    /// ```
    #[track_caller]
    fn with_latest_from<U, W2>(&self, other: &W2) -> Cell<(T, U), CellImmutable>
    where
        T: CellValue,
        U: CellValue,
        W2: Gettable<U> + Clone + Send + Sync + 'static,
        Self: Clone + Send + Sync + 'static,
    {
        // When self emits, pair with latest from other
        let other = other.clone();
        self.map(move |t| (t.clone(), other.get())).materialize()
    }
}

impl<T, W: Watchable<T>> WithLatestFromExt<T> for W {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Mutable, Signal};

    #[test]
    fn test_with_latest_from() {
        let source = Cell::new(0);
        let other = Cell::new("a".to_string());
        let combined = source.with_latest_from(&other);

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
