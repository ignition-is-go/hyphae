use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use crate::cell::{Cell, CellImmutable, CellMutable};
use super::{Gettable, Watchable};

pub trait JoinExt<T>: Watchable<T> {
    fn join<U, M>(&self, other: &Cell<U, M>) -> Cell<(T, U), CellImmutable>
    where
        T: Clone + Send + Sync + 'static,
        U: Clone + Send + Sync + 'static,
        M: Send + Sync + 'static,
        Self: Clone + Send + Sync + 'static,
    {
        let initial = (self.get(), other.get());
        let derived = Cell::<(T, U), CellMutable>::new(initial);

        // Subscribe to self
        let weak1 = derived.downgrade();
        let other1 = other.clone();
        let first1 = Arc::new(AtomicBool::new(true));
        let guard1 = self.subscribe(move |a| {
            if first1.swap(false, Ordering::SeqCst) {
                return;
            }
            if let Some(d) = weak1.upgrade() {
                d.notify((a.clone(), other1.get()));
            }
        });
        derived.own(guard1);

        // Subscribe to other
        let weak2 = derived.downgrade();
        let self2 = self.clone();
        let first2 = Arc::new(AtomicBool::new(true));
        let guard2 = other.subscribe(move |b| {
            if first2.swap(false, Ordering::SeqCst) {
                return;
            }
            if let Some(d) = weak2.upgrade() {
                d.notify((self2.get(), b.clone()));
            }
        });
        derived.own(guard2);

        // Complete when BOTH sources complete
        let self_complete = Arc::new(AtomicBool::new(false));
        let other_complete = Arc::new(AtomicBool::new(false));

        let weak = derived.downgrade();
        let sc = self_complete.clone();
        let oc = other_complete.clone();
        let complete_guard1 = self.on_complete(move || {
            sc.store(true, Ordering::SeqCst);
            if oc.load(Ordering::SeqCst)
                && let Some(d) = weak.upgrade() {
                    d.complete();
                }
        });
        derived.own(complete_guard1);

        let weak = derived.downgrade();
        let complete_guard2 = other.on_complete(move || {
            other_complete.store(true, Ordering::SeqCst);
            if self_complete.load(Ordering::SeqCst)
                && let Some(d) = weak.upgrade() {
                    d.complete();
                }
        });
        derived.own(complete_guard2);

        derived.lock()
    }
}

impl<T, W: Watchable<T>> JoinExt<T> for W {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MapExt, Mutable, flat};

    #[test]
    fn test_join_combines_cells() {
        let a = Cell::new(1);
        let b = Cell::new("hello");

        let joined = a.join(&b);
        assert_eq!(joined.get(), (1, "hello"));

        a.set(2);
        assert_eq!(joined.get(), (2, "hello"));

        b.set("world");
        assert_eq!(joined.get(), (2, "world"));
    }

    #[test]
    fn test_flat_macro_chain() {
        let a = Cell::new(1);
        let b = Cell::new(2);
        let c = Cell::new(3);
        let d = Cell::new(4);

        // flat!(|a, b, c, d| ...) expands to |(((a, b), c), d)| ...
        let sum = a.join(&b).join(&c).join(&d).map(flat!(|a, b, c, d| a + b + c + d));

        assert_eq!(sum.get(), 10);
    }
}
