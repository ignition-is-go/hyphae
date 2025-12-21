use std::sync::Arc;
use crate::cell::{Cell, CellImmutable};
use super::{DepNode, Watchable};

/// Extension trait for fallible transformations.
pub trait TryMapExt<T>: Watchable<T> {
    /// Transform values with a fallible function.
    ///
    /// Returns a `Cell<Result<U, E>>` that contains `Ok(value)` when the
    /// transform succeeds, or `Err(error)` when it fails.
    ///
    /// # Example
    /// ```
    /// use rx3::{Cell, Mutable, TryMapExt, Gettable};
    ///
    /// let source = Cell::new(10i32);
    /// let parsed = source.try_map(|v| {
    ///     if *v > 0 {
    ///         Ok(v.to_string())
    ///     } else {
    ///         Err("must be positive")
    ///     }
    /// });
    ///
    /// assert_eq!(parsed.get(), Ok("10".to_string()));
    /// ```
    fn try_map<U, E, F>(&self, f: F) -> Cell<Result<U, E>, CellImmutable>
    where
        U: Clone + Send + Sync + 'static,
        E: Clone + Send + Sync + 'static,
        F: Fn(&T) -> Result<U, E> + Send + Sync + 'static,
    {
        let initial = f(&self.get());
        let parent: Arc<dyn DepNode> = Arc::new(self.clone());
        let derived = Cell::<Result<U, E>, CellImmutable>::derived(initial, vec![parent]);

        let d = derived.clone();
        self.watch(move |value| {
            d.notify(f(value));
        });

        derived
    }
}

impl<T, W: Watchable<T>> TryMapExt<T> for W {}
