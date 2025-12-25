use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::Duration;
use crate::cell::{Cell, CellImmutable, CellMutable};
use crate::signal::Signal;
use super::Watchable;

pub trait DebounceExt<T>: Watchable<T> {
    fn debounce(&self, duration: Duration) -> Cell<T, CellImmutable>
    where
        T: Clone + Send + Sync + 'static,
        Self: Clone + Send + Sync + 'static,
    {
        let cell = Cell::<T, CellMutable>::new(self.get());

        let generation = Arc::new(AtomicU64::new(0));
        let weak = cell.downgrade();
        let guard = self.subscribe(move |signal| {
            match signal {
                Signal::Value(value, _) => {
                    let my_gen = generation.fetch_add(1, Ordering::SeqCst) + 1;
                    let value = value.clone(); // Arc clone
                    let weak = weak.clone();
                    let generation = generation.clone();

                    thread::spawn(move || {
                        thread::sleep(duration);
                        if generation.load(Ordering::SeqCst) == my_gen
                            && let Some(c) = weak.upgrade()
                        {
                            c.notify(Signal::value_arc(value));
                        }
                    });
                }
                Signal::Complete => {
                    if let Some(c) = weak.upgrade() {
                        c.notify(Signal::Complete);
                    }
                }
                Signal::Error(e) => {
                    if let Some(c) = weak.upgrade() {
                        c.notify(Signal::Error(e.clone()));
                    }
                }
            }
        });
        cell.own(guard);

        cell.lock()
    }
}

impl<T, W: Watchable<T>> DebounceExt<T> for W {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Mutable;
    use std::sync::atomic::AtomicU64;

    #[test]
    fn test_debounce_waits_for_pause() {
        let source = Cell::new(0u64);
        let debounced = source.debounce(Duration::from_millis(50));
        let received = Arc::new(AtomicU64::new(0));

        let r = received.clone();
        let _guard = debounced.subscribe(move |signal| {
            if let Signal::Value(v, _) = signal {
                r.store(**v, Ordering::SeqCst);
            }
        });

        // Rapid updates
        source.set(1);
        source.set(2);
        source.set(3);

        // Should not have updated yet
        thread::sleep(Duration::from_millis(10));
        assert_eq!(received.load(Ordering::SeqCst), 0);

        // Wait for debounce
        thread::sleep(Duration::from_millis(100));
        assert_eq!(received.load(Ordering::SeqCst), 3);
    }
}
