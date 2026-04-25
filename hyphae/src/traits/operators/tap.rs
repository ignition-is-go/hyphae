use super::{CellValue, MapExt, Watchable};
use crate::{
    cell::{Cell, CellImmutable},
    pipeline::Pipeline,
};

pub trait TapExt<T>: Watchable<T> {
    /// Perform a side effect for each value without modifying it.
    /// Equivalent to `map(|x| { f(x); x.clone() })`.
    #[track_caller]
    fn tap(&self, f: impl Fn(&T) + Send + Sync + 'static) -> Cell<T, CellImmutable>
    where
        T: CellValue,
        Self: Clone + Send + Sync + 'static,
    {
        self.map(move |x| {
            f(x);
            x.clone()
        })
        .materialize()
    }
}

impl<T, W: Watchable<T>> TapExt<T> for W {}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    };

    use super::*;
    use crate::{Gettable, Mutable};

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
