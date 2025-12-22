use crate::cell::{Cell, CellImmutable};

use super::{Gettable, Watchable};
use super::super::operators::MapExt;

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
    /// use rx3::{Cell, Mutable, Gettable, WithLatestFromExt};
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
    fn with_latest_from<U, W2>(&self, other: &W2) -> Cell<(T, U), CellImmutable>
    where
        T: Clone + Send + Sync + 'static,
        U: Clone + Send + Sync + 'static,
        W2: Gettable<U> + Clone + Send + Sync + 'static,
        Self: Clone + Send + Sync + 'static,
    {
        // When self emits, pair with latest from other
        let other = other.clone();
        self.map(move |t| (t.clone(), other.get()))
    }
}

impl<T, W: Watchable<T>> WithLatestFromExt<T> for W {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Mutable, Signal};
    use std::sync::Mutex;

    #[test]
    fn test_with_latest_from() {
        let source = Cell::new(0);
        let other = Cell::new("a".to_string());
        let combined = source.with_latest_from(&other);

        let emissions = std::sync::Arc::new(Mutex::new(Vec::new()));
        let e = emissions.clone();
        let _guard = combined.subscribe(move |signal| {
            if let Signal::Value(v) = signal {
                e.lock().unwrap().push((**v).clone());
            }
        });

        // Initial combined value
        assert_eq!(emissions.lock().unwrap().clone(), vec![(0, "a".to_string())]);

        // Other changes - no emission
        other.set("b".to_string());
        other.set("c".to_string());
        assert_eq!(emissions.lock().unwrap().len(), 1);

        // Source changes - emits with latest other
        source.set(1);
        assert_eq!(emissions.lock().unwrap().clone(), vec![
            (0, "a".to_string()),
            (1, "c".to_string()),
        ]);

        // Other changes again - no emission
        other.set("d".to_string());
        assert_eq!(emissions.lock().unwrap().len(), 2);

        // Source changes - emits with latest other
        source.set(2);
        assert_eq!(emissions.lock().unwrap().clone(), vec![
            (0, "a".to_string()),
            (1, "c".to_string()),
            (2, "d".to_string()),
        ]);
    }
}
