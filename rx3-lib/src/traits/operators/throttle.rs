use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;
use crate::cell::{Cell, CellImmutable, CellMutable};
use crate::signal::Signal;
use super::Watchable;

pub trait ThrottleExt<T>: Watchable<T> {
    fn throttle(&self, duration: Duration) -> Cell<T, CellImmutable>
    where
        T: Clone + Send + Sync + 'static,
        Self: Clone + Send + Sync + 'static,
    {
        let cell = Cell::<T, CellMutable>::new(self.get());

        let can_emit = Arc::new(AtomicBool::new(true));
        let weak = cell.downgrade();
        let guard = self.subscribe(move |signal| {
            if let Some(c) = weak.upgrade() {
                match signal {
                    Signal::Value(value) => {
                        if can_emit.swap(false, Ordering::SeqCst) {
                            c.notify(Signal::Value(value.clone()));

                            let can_emit = can_emit.clone();
                            thread::spawn(move || {
                                thread::sleep(duration);
                                can_emit.store(true, Ordering::SeqCst);
                            });
                        }
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

impl<T, W: Watchable<T>> ThrottleExt<T> for W {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Mutable;
    use std::sync::atomic::AtomicU64;

    #[test]
    fn test_throttle_limits_rate() {
        let source = Cell::new(0u64);
        let throttled = source.throttle(Duration::from_millis(50));
        let count = Arc::new(AtomicU64::new(0));

        let c = count.clone();
        let _guard = throttled.subscribe(move |_| {
            c.fetch_add(1, Ordering::SeqCst);
        });

        // Rapid updates
        for i in 1..=10 {
            source.set(i);
        }

        // Should have limited emissions
        let emissions = count.load(Ordering::SeqCst);
        assert!(emissions < 10, "throttle should limit emissions, got {}", emissions);
    }
}
