use std::thread;
use std::time::Duration;
use crate::cell::{Cell, CellImmutable, CellMutable};
use super::Watchable;

pub trait DelayExt<T>: Watchable<T> {
    fn delay(&self, duration: Duration) -> Cell<T, CellImmutable>
    where
        T: Clone + Send + Sync + 'static,
        Self: Clone + Send + Sync + 'static,
    {
        let cell = Cell::<T, CellMutable>::new(self.get());

        let weak = cell.downgrade();
        let guard = self.subscribe(move |signal| {
            let signal = signal.clone();
            let weak = weak.clone();
            thread::spawn(move || {
                thread::sleep(duration);
                if let Some(c) = weak.upgrade() {
                    c.notify(signal);
                }
            });
        });
        cell.own(guard);

        cell.lock()
    }
}

impl<T, W: Watchable<T>> DelayExt<T> for W {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Mutable, Signal};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[test]
    fn test_delay_delays_emission() {
        let source = Cell::new(0u64);
        let delayed = source.delay(Duration::from_millis(50));
        let received = Arc::new(AtomicU64::new(0));

        let r = received.clone();
        let _guard = delayed.subscribe(move |signal| {
            if let Signal::Value(v) = signal {
                r.store(*v, Ordering::SeqCst);
            }
        });

        // Wait for the initial delayed value (0) to arrive before triggering a new one.
        thread::sleep(Duration::from_millis(100));
        assert_eq!(received.load(Ordering::SeqCst), 0);

        source.set(42);

        // Not yet (delay is 50ms, so after 20ms value should still be 0)
        thread::sleep(Duration::from_millis(20));
        assert_eq!(received.load(Ordering::SeqCst), 0);

        // Now (wait 100ms more to ensure delay has passed with margin for thread scheduling)
        thread::sleep(Duration::from_millis(100));
        assert_eq!(received.load(Ordering::SeqCst), 42);
    }
}
