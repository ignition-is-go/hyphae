use std::sync::Arc;
use std::thread;
use std::time::Duration;
use crate::cell::{Cell, CellImmutable};
use super::{DepNode, Watchable};

pub trait DelayExt<T>: Watchable<T> {
    fn delay(&self, duration: Duration) -> Cell<T, CellImmutable>
    where
        T: Clone + Send + Sync + 'static,
        Self: Clone + Send + Sync + 'static,
    {
        let parent: Arc<dyn DepNode> = Arc::new(self.clone());
        let cell = Cell::<T, CellImmutable>::derived(self.get(), vec![parent]);

        let weak = cell.downgrade();
        let guard = self.subscribe(move |value| {
            let value = value.clone();
            let weak = weak.clone();
            thread::spawn(move || {
                thread::sleep(duration);
                if let Some(c) = weak.upgrade() {
                    c.notify(value);
                }
            });
        });
        cell.own(guard);

        cell
    }
}

impl<T, W: Watchable<T>> DelayExt<T> for W {}
