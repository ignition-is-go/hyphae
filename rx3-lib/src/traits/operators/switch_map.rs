use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use arc_swap::ArcSwap;
use crate::cell::{Cell, CellImmutable};
use crate::subscription::SubscriptionGuard;
use super::{Gettable, Watchable};

pub trait SwitchMapExt<T>: Watchable<T> {
    fn switch_map<U, F>(&self, f: F) -> Cell<U, CellImmutable>
    where
        T: Clone + Send + Sync + 'static,
        U: Clone + Send + Sync + 'static,
        F: Fn(&T) -> Cell<U, CellImmutable> + Send + Sync + 'static,
        Self: Clone + Send + Sync + 'static,
    {
        let first_inner = f(&self.get());
        let cell = Cell::<U, CellImmutable>::derived(first_inner.get(), vec![]);

        // Subscribe to first inner
        let first_guard = {
            let weak = cell.downgrade();
            first_inner.subscribe(move |value| {
                if let Some(c) = weak.upgrade() {
                    c.notify(value.clone());
                }
            })
        };

        // Track current subscription (lock-free)
        // When we swap in a new guard, the old one drops and auto-unsubscribes
        let current: Arc<ArcSwap<Option<SubscriptionGuard>>> =
            Arc::new(ArcSwap::from_pointee(Some(first_guard)));

        // When outer changes, switch to new inner
        let weak_outer = cell.downgrade();
        let f = Arc::new(f);
        let first = Arc::new(AtomicBool::new(true));
        let current_clone = current.clone();
        let outer_guard = self.subscribe(move |outer_value| {
            if first.swap(false, Ordering::SeqCst) {
                return;
            }

            let Some(_) = weak_outer.upgrade() else { return };

            let inner = f(outer_value);

            // Subscribe to new inner
            let weak_inner = weak_outer.clone();
            let new_guard = inner.subscribe(move |value| {
                if let Some(c) = weak_inner.upgrade() {
                    c.notify(value.clone());
                }
            });

            // Swap in new guard - old guard drops and unsubscribes automatically
            current_clone.store(Arc::new(Some(new_guard)));
        });
        cell.own(outer_guard);

        // When derived cell drops, this Arc drops, the guard inside drops,
        // and it auto-unsubscribes from the current inner cell
        cell.own(current);

        cell
    }
}

impl<T, W: Watchable<T>> SwitchMapExt<T> for W {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MapExt;

    #[test]
    fn test_switch_map_switches() {
        let source = Cell::new(1u64);
        let switched = source.switch_map(|v| {
            let v = *v;
            Cell::new(v * 10).map(move |x| x + v)
        });

        // Initial: 1 * 10 + 1 = 11
        assert_eq!(switched.get(), 11);
    }
}
