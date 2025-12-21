use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use arc_swap::ArcSwap;
use uuid::Uuid;
use crate::cell::{Cell, CellImmutable};
use super::Watchable;

struct Subscription<U> {
    cell: Cell<U, CellImmutable>,
    id: Uuid,
}

pub trait SwitchMapExt<T>: Watchable<T> {
    fn switch_map<U, F>(&self, f: F) -> Cell<U, CellImmutable>
    where
        T: Clone + Send + Sync + 'static,
        U: Clone + Send + Sync + 'static,
        F: Fn(&T) -> Cell<U, CellImmutable> + Send + Sync + 'static,
    {
        let first_inner = f(&self.get());
        let cell = Cell::<U, CellImmutable>::derived(first_inner.get(), vec![]);

        // Subscribe to first inner
        let sub_id = {
            let cell = cell.clone();
            first_inner.watch(move |value| {
                cell.notify(value.clone());
            })
        };

        // Track current subscription (lock-free)
        let current_sub: Arc<ArcSwap<Option<Subscription<U>>>> =
            Arc::new(ArcSwap::from_pointee(Some(Subscription { cell: first_inner, id: sub_id })));

        // When outer changes, switch to new inner
        let cell_outer = cell.clone();
        let f = Arc::new(f);
        let first = Arc::new(AtomicBool::new(true));
        self.watch(move |outer_value| {
            // Skip first emission - we already handled it above
            if first.swap(false, Ordering::SeqCst) {
                return;
            }

            // Unsubscribe from previous inner
            if let Some(old) = current_sub.load().as_ref() {
                old.cell.unsubscribe(old.id);
            }

            let inner = f(outer_value);

            // Subscribe to new inner
            let cell_inner = cell_outer.clone();
            let sub_id = inner.watch(move |value| {
                cell_inner.notify(value.clone());
            });

            current_sub.store(Arc::new(Some(Subscription { cell: inner, id: sub_id })));
        });

        cell
    }
}

impl<T, W: Watchable<T>> SwitchMapExt<T> for W {}
