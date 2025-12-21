use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::Duration;
use crate::cell::{Cell, CellImmutable};
use super::{DepNode, Watchable};

pub trait DebounceExt<T>: Watchable<T> {
    fn debounce(&self, duration: Duration) -> Cell<T, CellImmutable>
    where
        T: Clone + Send + Sync + 'static,
        Self: Clone + Send + Sync + 'static,
    {
        let parent: Arc<dyn DepNode> = Arc::new(self.clone());
        let cell = Cell::<T, CellImmutable>::derived(self.get(), vec![parent]);

        let generation = Arc::new(AtomicU64::new(0));
        let weak = cell.downgrade();
        let guard = self.subscribe(move |value| {
            let my_gen = generation.fetch_add(1, Ordering::SeqCst) + 1;
            let value = value.clone();
            let weak = weak.clone();
            let generation = generation.clone();

            thread::spawn(move || {
                thread::sleep(duration);
                if generation.load(Ordering::SeqCst) == my_gen {
                    if let Some(c) = weak.upgrade() {
                        c.notify(value);
                    }
                }
            });
        });
        cell.own(guard);

        cell
    }
}

impl<T, W: Watchable<T>> DebounceExt<T> for W {}
