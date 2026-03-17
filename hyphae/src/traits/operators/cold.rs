use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use super::{CellValue, Watchable};
use crate::{
    cell::{Cell, CellImmutable, CellMutable},
    signal::Signal,
};

pub trait ColdExt<T>: Watchable<T> {
    /// Creates a "cold" cell that starts as `None` and only emits `Some(value)`
    /// for new emissions after creation.
    ///
    /// This is useful for trigger/event-style cells where you don't want to
    /// react to the source's retained value — only to fresh emissions.
    ///
    /// When used inside `switch_map`, each re-creation gets a fresh cold cell,
    /// providing per-reconnection suppression of retained values.
    ///
    /// Lock-free: the inner value is wrapped in `Arc` so forwarding is just
    /// an atomic refcount increment — no deep clone of `T`.
    #[track_caller]
    fn cold(&self) -> Cell<Option<Arc<T>>, CellImmutable>
    where
        T: CellValue,
        Self: Clone + Send + Sync + 'static,
    {
        let cell = Cell::<Option<Arc<T>>, CellMutable>::new(None);
        let cell = if let Some(name) = self.name() {
            cell.with_name(format!("{}::cold", name))
        } else {
            cell
        };

        let weak = cell.downgrade();
        let first = Arc::new(AtomicBool::new(true));
        let guard = self.subscribe(move |signal| {
            if let Some(c) = weak.upgrade() {
                match signal {
                    Signal::Value(value) => {
                        if first.swap(false, Ordering::SeqCst) {
                            return;
                        }
                        // Arc::clone is just an atomic refcount increment — no deep copy
                        c.notify(Signal::value(Some(value.clone())));
                    }
                    Signal::Complete => c.notify(Signal::Complete),
                    Signal::Error(e) => c.notify(Signal::Error(e.clone())),
                }
            }
        });
        cell.own(guard);

        cell.lock()
    }
}

impl<T, W: Watchable<T>> ColdExt<T> for W {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Gettable, Mutable};

    #[test]
    fn test_cold_starts_as_none() {
        let source = Cell::new(42u64);
        let cold = source.cold();

        assert_eq!(cold.get(), None);
    }

    #[test]
    fn test_cold_emits_some_on_change() {
        let source = Cell::new(42u64);
        let cold = source.cold();

        assert_eq!(cold.get(), None);

        source.set(100);
        assert_eq!(cold.get(), Some(Arc::new(100)));
    }

    #[test]
    fn test_cold_does_not_replay_retained_value() {
        use std::sync::atomic::{AtomicU64, Ordering};

        let source = Cell::new(42u64);
        let cold = source.cold();
        let emission_count = Arc::new(AtomicU64::new(0));

        let count = emission_count.clone();
        let _guard = cold.subscribe(move |signal| {
            if let Signal::Value(_) = signal {
                count.fetch_add(1, Ordering::SeqCst);
            }
        });

        // Subscribe fires once with initial None value
        assert_eq!(emission_count.load(Ordering::SeqCst), 1);
        assert_eq!(cold.get(), None); // Retained source value (42) was NOT replayed

        // Source change emits Some
        source.set(100);
        assert_eq!(emission_count.load(Ordering::SeqCst), 2);
        assert_eq!(cold.get(), Some(Arc::new(100)));

        source.set(200);
        assert_eq!(emission_count.load(Ordering::SeqCst), 3);
        assert_eq!(cold.get(), Some(Arc::new(200)));
    }

    #[test]
    fn test_cold_inside_switch_map_skips_per_reconnection() {
        use crate::SwitchMapExt;

        let selector = Cell::new(1u64);
        let source_a = Cell::new(10u64);
        let source_b = Cell::new(20u64);

        let a_clone = source_a.clone();
        let b_clone = source_b.clone();
        let result = selector.switch_map(move |sel| {
            if *sel == 1 {
                a_clone.cold()
            } else {
                b_clone.cold()
            }
        });

        // Initial: cold starts as None
        assert_eq!(result.get(), None);

        // Source A emits — result gets Some
        source_a.set(11);
        assert_eq!(result.get(), Some(Arc::new(11)));

        // Switch to source B — fresh cold, starts as None
        selector.set(2);
        assert_eq!(result.get(), None);

        // Source B emits — result gets Some
        source_b.set(21);
        assert_eq!(result.get(), Some(Arc::new(21)));
    }
}
