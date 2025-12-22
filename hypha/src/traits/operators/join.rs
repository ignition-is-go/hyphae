use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use crate::cell::{Cell, CellImmutable, CellMutable};
use crate::signal::Signal;
use super::{Gettable, Watchable};

// Completion state flags for join (both must complete)
const SELF_COMPLETE: u8 = 0b01;
const OTHER_COMPLETE: u8 = 0b10;

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

        let complete_state = Arc::new(AtomicU8::new(0));

        // Subscribe to self
        let weak1 = derived.downgrade();
        let other1 = other.clone();
        let first1 = Arc::new(AtomicBool::new(true));
        let cs1 = complete_state.clone();
        let guard1 = self.subscribe(move |signal| {
            if let Some(d) = weak1.upgrade() {
                match signal {
                    Signal::Value(a) => {
                        if first1.swap(false, Ordering::SeqCst) {
                            return;
                        }
                        d.notify(Signal::value((a.as_ref().clone(), other1.get())));
                    }
                    Signal::Complete => {
                        let prev = cs1.fetch_or(SELF_COMPLETE, Ordering::SeqCst);
                        if prev == OTHER_COMPLETE {
                            d.notify(Signal::Complete);
                        }
                    }
                    Signal::Error(e) => d.notify(Signal::Error(e.clone())),
                }
            }
        });
        derived.own(guard1);

        // Subscribe to other
        let weak2 = derived.downgrade();
        let self2 = self.clone();
        let first2 = Arc::new(AtomicBool::new(true));
        let guard2 = other.subscribe(move |signal| {
            if let Some(d) = weak2.upgrade() {
                match signal {
                    Signal::Value(b) => {
                        if first2.swap(false, Ordering::SeqCst) {
                            return;
                        }
                        d.notify(Signal::value((self2.get(), b.as_ref().clone())));
                    }
                    Signal::Complete => {
                        let prev = complete_state.fetch_or(OTHER_COMPLETE, Ordering::SeqCst);
                        if prev == SELF_COMPLETE {
                            d.notify(Signal::Complete);
                        }
                    }
                    Signal::Error(e) => d.notify(Signal::Error(e.clone())),
                }
            }
        });
        derived.own(guard2);

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
