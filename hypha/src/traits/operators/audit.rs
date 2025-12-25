use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use arc_swap::ArcSwap;

use crate::cell::{Cell, CellImmutable, CellMutable};
use crate::signal::Signal;

use super::Watchable;

pub trait AuditExt<T>: Watchable<T> {
    /// Like throttle but emits the LAST value in the window.
    ///
    /// Silences during the window, then emits the most recent value
    /// when the window expires.
    ///
    /// # Example
    ///
    /// ```
    /// use hypha::{Cell, Mutable, AuditExt, Watchable};
    /// use std::time::Duration;
    ///
    /// let source = Cell::new(0);
    /// let audited = source.audit(Duration::from_millis(100));
    ///
    /// source.set(1);
    /// source.set(2);
    /// source.set(3);
    /// // After 100ms, emits 3 (the last value)
    /// ```
    fn audit(&self, duration: Duration) -> Cell<T, CellImmutable>
    where
        T: Clone + Send + Sync + 'static,
        Self: Clone + Send + Sync + 'static,
    {
        let derived = Cell::<T, CellMutable>::new(self.get());

        let weak = derived.downgrade();
        let first = Arc::new(AtomicBool::new(true));
        let latest: Arc<ArcSwap<Option<T>>> = Arc::new(ArcSwap::from_pointee(None));
        let generation = Arc::new(AtomicU64::new(0));
        let in_window = Arc::new(AtomicBool::new(false));

        let guard = self.subscribe(move |signal| {
            if let Some(d) = weak.upgrade() {
                match signal {
                    Signal::Value(value, _) => {
                        if first.swap(false, Ordering::SeqCst) {
                            return;
                        }

                        // Store latest value
                        latest.store(Arc::new(Some((**value).clone())));

                        // If not in a window, start one
                        if !in_window.swap(true, Ordering::SeqCst) {
                            let current_gen = generation.fetch_add(1, Ordering::SeqCst) + 1;
                            let latest2 = latest.clone();
                            let weak2 = d.downgrade();
                            let gen_ref = generation.clone();
                            let in_win = in_window.clone();

                            thread::spawn(move || {
                                thread::sleep(duration);
                                // Only emit if this is still the current window
                                if gen_ref.load(Ordering::SeqCst) == current_gen {
                                    if let Some(d2) = weak2.upgrade() {
                                        let val = latest2.load();
                                        if let Some(v) = &**val {
                                            d2.notify(Signal::value(v.clone()));
                                        }
                                    }
                                    in_win.store(false, Ordering::SeqCst);
                                }
                            });
                        }
                    }
                    Signal::Complete => d.notify(Signal::Complete),
                    Signal::Error(e) => d.notify(Signal::Error(e.clone())),
                }
            }
        });
        derived.own(guard);

        derived.lock()
    }
}

impl<T, W: Watchable<T>> AuditExt<T> for W {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Gettable, Mutable};
    use std::sync::atomic::AtomicU32;

    #[test]
    fn test_audit_emits_last() {
        let source = Cell::new(0);
        let audited = source.audit(Duration::from_millis(50));

        let emissions = Arc::new(AtomicU32::new(0));
        let e = emissions.clone();
        let _guard = audited.subscribe(move |signal| {
            if let Signal::Value(_, _) = signal {
                e.fetch_add(1, Ordering::SeqCst);
            }
        });

        assert_eq!(emissions.load(Ordering::SeqCst), 1); // Initial

        // Rapid emissions
        source.set(1);
        source.set(2);
        source.set(3);

        // Should not emit immediately
        assert_eq!(emissions.load(Ordering::SeqCst), 1);

        // Wait for audit window
        thread::sleep(Duration::from_millis(70));

        // Should have emitted once (the last value)
        assert_eq!(emissions.load(Ordering::SeqCst), 2);
        assert_eq!(audited.get(), 3);
    }
}
