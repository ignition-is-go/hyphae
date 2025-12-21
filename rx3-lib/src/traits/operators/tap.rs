use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use crate::cell::{Cell, CellImmutable};
use super::{DepNode, Watchable};

pub trait TapExt<T>: Watchable<T> {
    fn tap(&self, f: impl Fn(&T) + Send + Sync + 'static) -> Cell<T, CellImmutable>
    where
        T: Clone + Send + Sync + 'static,
        Self: Clone + Send + Sync + 'static,
    {
        let parent: Arc<dyn DepNode> = Arc::new(self.clone());
        let cell = Cell::<T, CellImmutable>::derived(self.get(), vec![parent]);

        let weak = cell.downgrade();
        let first = Arc::new(AtomicBool::new(true));
        let guard = self.subscribe(move |value| {
            if first.swap(false, Ordering::SeqCst) {
                return;
            }
            if let Some(c) = weak.upgrade() {
                f(value);
                c.notify(value.clone());
            }
        });
        cell.own(guard);

        cell
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
