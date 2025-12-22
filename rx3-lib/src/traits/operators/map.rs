use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use crate::cell::{Cell, CellImmutable, CellMutable};
use crate::signal::Signal;
use super::Watchable;

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

        let derived = Cell::<U, CellMutable>::new(initial);
        let derived = if let Some(parent_name) = self.name() {
            derived.with_name(format!("{}::map", parent_name))
        } else {
            derived
        };

        let weak = derived.downgrade();
        let first = Arc::new(AtomicBool::new(true));
        let guard = self.subscribe(move |signal| {
            if let Some(d) = weak.upgrade() {
                match signal {
                    Signal::Value(value) => {
                        if first.swap(false, Ordering::SeqCst) {
                            return;
                        }
                        d.notify(Signal::Value(transform(value)));
                    }
                    Signal::Complete => d.notify(Signal::Complete),
                    Signal::Error(e) => d.notify(Signal::Error(e.clone())),
                }
            }
        });
        derived.own(guard);

        derived.lock()
    }
}

impl<T, W: Watchable<T>> MapExt<T> for W {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::DepNode;
    use crate::{Gettable, Mutable};

    #[test]
    fn test_map_transform() {
        let source = Cell::new(5);
        let doubled = source.map(|x| x * 2);

        assert_eq!(doubled.get(), 10);

        source.set(10);
        assert_eq!(doubled.get(), 20);
    }

    #[test]
    fn test_map_chain() {
        let source = Cell::new(1);
        let result = source
            .map(|x| x + 1)
            .map(|x| x * 2)
            .map(|x| x + 10);

        assert_eq!(result.get(), 14); // ((1 + 1) * 2) + 10

        source.set(5);
        assert_eq!(result.get(), 22); // ((5 + 1) * 2) + 10
    }

    #[test]
    fn test_map_type_change() {
        let source = Cell::new(42);
        let stringified = source.map(|x| format!("value: {}", x));

        assert_eq!(stringified.get(), "value: 42");
    }

    #[test]
    fn test_map_tracks_dependency() {
        let source = Cell::new(1).with_name("source");
        let mapped = source.map(|x| x * 2);

        assert_eq!(mapped.dependency_count(), 1);
        assert!(mapped.has_dependencies());
    }
}
