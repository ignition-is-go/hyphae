use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;
use crate::cell::{Cell, CellImmutable};
use super::{DepNode, Watchable};

pub trait ThrottleExt<T>: Watchable<T> {
    fn throttle(&self, duration: Duration) -> Cell<T, CellImmutable>
    where
        T: Clone + Send + Sync + 'static,
        Self: Clone + Send + Sync + 'static,
    {
        let parent: Arc<dyn DepNode> = Arc::new(self.clone());
        let cell = Cell::<T, CellImmutable>::derived(self.get(), vec![parent]);

        let can_emit = Arc::new(AtomicBool::new(true));
        let weak = cell.downgrade();
        let guard = self.subscribe(move |value| {
            if let Some(c) = weak.upgrade() {
                if can_emit.swap(false, Ordering::SeqCst) {
                    c.notify(value.clone());

                    let can_emit = can_emit.clone();
                    thread::spawn(move || {
                        thread::sleep(duration);
                        can_emit.store(true, Ordering::SeqCst);
                    });
                }
            }
        });
        cell.own(guard);

        cell
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
