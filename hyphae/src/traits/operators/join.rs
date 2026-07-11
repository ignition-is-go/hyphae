use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU8, Ordering},
};

use parking_lot::Mutex;

use super::{CellValue, Gettable, Watchable};
use crate::{
    cell::{Cell, CellImmutable, CellMutable},
    signal::Signal,
};

// Completion state flags for join (both must complete)
const SELF_COMPLETE: u8 = 0b01;
const OTHER_COMPLETE: u8 = 0b10;

pub trait JoinExt<T>: Watchable<T> {
    #[track_caller]
    fn join<U, M>(&self, other: &Cell<U, M>) -> Cell<(T, U), CellImmutable>
    where
        T: CellValue,
        U: CellValue,
        M: Send + Sync + 'static,
        Self: Clone + Send + Sync + 'static,
    {
        let initial = (self.get(), other.get());
        // Shared "last known values" for both sides, held through the ENTIRE
        // update-then-notify sequence below (not just the read that builds
        // the combined tuple). This matters only under the `scheduler`
        // feature's wave-parallel draining, where two same-height cells can
        // now notify at the literal same instant on different threads: each
        // side used to read the other's `.get()` independently, so whichever
        // notify's *push* into the scheduler's coalescing slot happened to
        // land last could carry a stale peek at a sibling that hadn't
        // updated yet, leaving a torn combined value as the survivor
        // (confirmed by repro: ~16% of concurrent same-height joins). Holding
        // this lock across the notify call, not just the read, guarantees
        // whichever side's push actually lands last also reflects the
        // freshest state of both sides — the other side can't sneak an
        // update in between "I read the combined pair" and "I pushed it".
        let latest: Arc<Mutex<(T, U)>> = Arc::new(Mutex::new(initial.clone()));
        let derived = Cell::<(T, U), CellMutable>::new(initial);
        let derived = if let Some(name) = self.name() {
            derived.with_name(format!("{}::join", name))
        } else {
            derived
        };

        let complete_state = Arc::new(AtomicU8::new(0));

        // Subscribe to self
        let weak1 = derived.downgrade();
        let first1 = Arc::new(AtomicBool::new(true));
        let cs1 = complete_state.clone();
        let latest1 = latest.clone();
        let guard1 = self.subscribe(move |signal| {
            if let Some(d) = weak1.upgrade() {
                match signal {
                    Signal::Value(a) => {
                        if first1.swap(false, Ordering::SeqCst) {
                            return;
                        }
                        let mut guard = latest1.lock();
                        guard.0 = a.as_ref().clone();
                        d.notify(Signal::value(guard.clone()));
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
        let first2 = Arc::new(AtomicBool::new(true));
        let latest2 = latest;
        let guard2 = other.subscribe(move |signal| {
            if let Some(d) = weak2.upgrade() {
                match signal {
                    Signal::Value(b) => {
                        if first2.swap(false, Ordering::SeqCst) {
                            return;
                        }
                        let mut guard = latest2.lock();
                        guard.1 = b.as_ref().clone();
                        d.notify(Signal::value(guard.clone()));
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
    use crate::{Gettable, MapExt, MaterializeDefinite, Mutable};

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
        let sum = a
            .join(&b)
            .join(&c)
            .join(&d)
            .map(flat!(|a, b, c, d| a + b + c + d))
            .materialize();

        assert_eq!(sum.get(), 10);
    }
}
