use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use crate::cell::{Cell, CellImmutable};
use super::{DepNode, Gettable, Watchable};

pub trait JoinExt<T>: Watchable<T> {
    fn join<U, M>(&self, other: &Cell<U, M>) -> Cell<(T, U), CellImmutable>
    where
        T: Clone + Send + Sync + 'static,
        U: Clone + Send + Sync + 'static,
        M: Send + Sync + 'static,
        Self: Clone + Send + Sync + 'static,
    {
        let initial = (self.get(), other.get());
        let deps: Vec<Arc<dyn DepNode>> = vec![
            Arc::new(self.clone()),
            Arc::new(other.clone()),
        ];
        let derived = Cell::<(T, U), CellImmutable>::derived(initial, deps);

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

        derived
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
