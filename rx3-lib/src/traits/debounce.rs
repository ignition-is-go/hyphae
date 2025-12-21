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
    {
        let parent: Arc<dyn DepNode> = Arc::new(self.clone());
        let cell = Cell::<T, CellImmutable>::derived(self.get(), vec![parent]);

        let generation = Arc::new(AtomicU64::new(0));
        let c = cell.clone();
        self.watch(move |value| {
            let my_gen = generation.fetch_add(1, Ordering::SeqCst) + 1;
            let value = value.clone();
            let c = c.clone();
            let generation = generation.clone();

            thread::spawn(move || {
                thread::sleep(duration);
                // Only emit if no newer value came in
                if generation.load(Ordering::SeqCst) == my_gen {
                    c.notify(value);
                }
            });
        });

        cell
    }
}

impl<T, W: Watchable<T>> DebounceExt<T> for W {}
