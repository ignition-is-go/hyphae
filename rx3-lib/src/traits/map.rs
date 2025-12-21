use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use crate::cell::{Cell, CellImmutable};
use super::{DepNode, SubscribeExt, Watchable};

pub trait MapExt<T>: Watchable<T> {
    fn map<U: Clone + Send + Sync + 'static>(
        &self,
        transform: impl Fn(&T) -> U + Send + Sync + 'static,
    ) -> Cell<U, CellImmutable>
    where
        T: Clone + 'static,
        Self: Clone + Send + Sync + 'static,
    {
        let initial = transform(&self.get());
        let parent: Arc<dyn DepNode> = Arc::new(self.clone());

        let derived = Cell::<U, CellImmutable>::derived(initial, vec![parent]);
        let derived = if let Some(parent_name) = self.name() {
            derived.with_name(format!("{}::map", parent_name))
        } else {
            derived
        };

        let weak = derived.downgrade();
        let first = Arc::new(AtomicBool::new(true));
        let guard = self.subscribe(move |value| {
            if first.swap(false, Ordering::SeqCst) {
                return;
            }
            if let Some(d) = weak.upgrade() {
                d.notify(transform(value));
            }
        });
        derived.own(guard);

        derived
    }
}

impl<T, W: Watchable<T>> MapExt<T> for W {}
