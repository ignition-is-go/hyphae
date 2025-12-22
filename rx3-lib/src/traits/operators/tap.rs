use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use crate::cell::{Cell, CellImmutable, CellMutable};
use crate::signal::Signal;
use super::Watchable;

pub trait TapExt<T>: Watchable<T> {
    fn tap(&self, f: impl Fn(&T) + Send + Sync + 'static) -> Cell<T, CellImmutable>
    where
        T: Clone + Send + Sync + 'static,
        Self: Clone + Send + Sync + 'static,
    {
        let cell = Cell::<T, CellMutable>::new(self.get());

        let weak = cell.downgrade();
        let first = Arc::new(AtomicBool::new(true));
        let guard = self.subscribe(move |signal| {
            if let Some(c) = weak.upgrade() {
                match signal {
                    Signal::Value(value) => {
                        if first.swap(false, Ordering::SeqCst) {
                            return;
                        }
                        f(value);
                        c.notify(Signal::Value(value.clone()));
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

impl<T, W: Watchable<T>> TapExt<T> for W {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Gettable, Mutable};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    #[test]
    fn test_tap_side_effect() {
        let source = Cell::new(0u64);
        let side_effect = Arc::new(AtomicU64::new(0));

        let se = side_effect.clone();
        let tapped = source.tap(move |v| {
            se.store(*v, Ordering::SeqCst);
        });

        source.set(42);
        assert_eq!(side_effect.load(Ordering::SeqCst), 42);
        assert_eq!(tapped.get(), 42); // value passes through unchanged
    }
}
