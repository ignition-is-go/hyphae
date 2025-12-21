use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use crate::cell::{Cell, CellImmutable};
use super::{DepNode, Gettable, Watchable};

pub trait DedupedExt<T>: Watchable<T> {
    fn deduped(&self) -> Cell<T, CellImmutable>
    where
        T: Clone + PartialEq + Send + Sync + 'static,
    {
        let parent: Arc<dyn DepNode> = Arc::new(self.clone());
        let derived = Cell::<T, CellImmutable>::derived(self.get(), vec![parent]);

        let weak = derived.downgrade();
        let first = Arc::new(AtomicBool::new(true));
        self.watch(move |value| {
            if first.swap(false, Ordering::SeqCst) {
                return;
            }
            if let Some(d) = weak.upgrade() {
                if *value != d.get() {
                    d.notify(value.clone());
                }
            }
        });

        derived
    }
}

impl<T, W: Watchable<T>> DedupedExt<T> for W {}
