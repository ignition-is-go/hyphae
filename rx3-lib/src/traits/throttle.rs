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
    {
        let parent: Arc<dyn DepNode> = Arc::new(self.clone());
        let cell = Cell::<T, CellImmutable>::derived(self.get(), vec![parent]);

        let can_emit = Arc::new(AtomicBool::new(true));
        let c = cell.clone();
        self.watch(move |value| {
            if can_emit.swap(false, Ordering::SeqCst) {
                c.notify(value.clone());

                let can_emit = can_emit.clone();
                thread::spawn(move || {
                    thread::sleep(duration);
                    can_emit.store(true, Ordering::SeqCst);
                });
            }
        });

        cell
    }
}

impl<T, W: Watchable<T>> ThrottleExt<T> for W {}
