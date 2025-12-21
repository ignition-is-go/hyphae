use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use crate::cell::{Cell, CellImmutable, CellMutable};
use super::Watchable;

/// Extension trait for transforming Ok values in Result cells.
pub trait MapOkExt<T, E>: Watchable<Result<T, E>> {
    /// Transform the Ok value, passing through Err unchanged.
    fn map_ok<U, F>(&self, f: F) -> Cell<Result<U, E>, CellImmutable>
    where
        T: Clone + 'static,
        U: Clone + Send + Sync + 'static,
        E: Clone + Send + Sync + 'static,
        F: Fn(&T) -> U + Send + Sync + 'static,
        Self: Clone + Send + Sync + 'static,
    {
        let initial = match &self.get() {
            Ok(v) => Ok(f(v)),
            Err(e) => Err(e.clone()),
        };
        let derived = Cell::<Result<U, E>, CellMutable>::new(initial);

        let weak = derived.downgrade();
        let first = Arc::new(AtomicBool::new(true));
        let guard = self.subscribe(move |result| {
            if first.swap(false, Ordering::SeqCst) {
                return;
            }
            if let Some(d) = weak.upgrade() {
                let mapped = match result {
                    Ok(v) => Ok(f(v)),
                    Err(e) => Err(e.clone()),
                };
                d.notify(mapped);
            }
        });
        derived.own(guard);

        // Propagate source completion
        let weak = derived.downgrade();
        let complete_guard = self.on_complete(move || {
            if let Some(d) = weak.upgrade() {
                d.complete();
            }
        });
        derived.own(complete_guard);

        derived.lock()
    }
}

impl<T, E, W> MapOkExt<T, E> for W
where
    T: Clone + Send + Sync + 'static,
    E: Clone + Send + Sync + 'static,
    W: Watchable<Result<T, E>>,
{}

/// Extension trait for transforming Err values in Result cells.
pub trait MapErrExt<T, E>: Watchable<Result<T, E>> {
    /// Transform the Err value, passing through Ok unchanged.
    fn map_err<E2, F>(&self, f: F) -> Cell<Result<T, E2>, CellImmutable>
    where
        T: Clone + Send + Sync + 'static,
        E: 'static,
        E2: Clone + Send + Sync + 'static,
        F: Fn(&E) -> E2 + Send + Sync + 'static,
        Self: Clone + Send + Sync + 'static,
    {
        let initial = match &self.get() {
            Ok(v) => Ok(v.clone()),
            Err(e) => Err(f(e)),
        };
        let derived = Cell::<Result<T, E2>, CellMutable>::new(initial);

        let weak = derived.downgrade();
        let first = Arc::new(AtomicBool::new(true));
        let guard = self.subscribe(move |result| {
            if first.swap(false, Ordering::SeqCst) {
                return;
            }
            if let Some(d) = weak.upgrade() {
                let mapped = match result {
                    Ok(v) => Ok(v.clone()),
                    Err(e) => Err(f(e)),
                };
                d.notify(mapped);
            }
        });
        derived.own(guard);

        // Propagate source completion
        let weak = derived.downgrade();
        let complete_guard = self.on_complete(move || {
            if let Some(d) = weak.upgrade() {
                d.complete();
            }
        });
        derived.own(complete_guard);

        derived.lock()
    }
}

impl<T, E, W> MapErrExt<T, E> for W
where
    T: Clone + Send + Sync + 'static,
    E: Clone + Send + Sync + 'static,
    W: Watchable<Result<T, E>>,
{}

/// Extension trait for recovering from errors.
pub trait CatchErrorExt<T, E>: Watchable<Result<T, E>> {
    /// Recover from errors by providing a fallback value.
    ///
    /// Converts `Cell<Result<T, E>>` to `Cell<T>` by handling errors.
    fn catch_error<F>(&self, f: F) -> Cell<T, CellImmutable>
    where
        T: Clone + Send + Sync + 'static,
        E: 'static,
        F: Fn(&E) -> T + Send + Sync + 'static,
        Self: Clone + Send + Sync + 'static,
    {
        let initial = match &self.get() {
            Ok(v) => v.clone(),
            Err(e) => f(e),
        };
        let derived = Cell::<T, CellMutable>::new(initial);

        let weak = derived.downgrade();
        let first = Arc::new(AtomicBool::new(true));
        let guard = self.subscribe(move |result| {
            if first.swap(false, Ordering::SeqCst) {
                return;
            }
            if let Some(d) = weak.upgrade() {
                let value = match result {
                    Ok(v) => v.clone(),
                    Err(e) => f(e),
                };
                d.notify(value);
            }
        });
        derived.own(guard);

        // Propagate source completion
        let weak = derived.downgrade();
        let complete_guard = self.on_complete(move || {
            if let Some(d) = weak.upgrade() {
                d.complete();
            }
        });
        derived.own(complete_guard);

        derived.lock()
    }
}

impl<T, E, W> CatchErrorExt<T, E> for W
where
    T: Clone + Send + Sync + 'static,
    E: Clone + Send + Sync + 'static,
    W: Watchable<Result<T, E>>,
{}

/// Extension trait for unwrapping Result cells with defaults.
pub trait UnwrapOrExt<T, E>: Watchable<Result<T, E>> {
    /// Unwrap Ok values, using a default for Err.
    fn unwrap_or(&self, default: T) -> Cell<T, CellImmutable>
    where
        T: Clone + Send + Sync + 'static,
        E: 'static,
        Self: Clone + Send + Sync + 'static,
    {
        let initial = match self.get() {
            Ok(v) => v,
            Err(_) => default.clone(),
        };
        let derived = Cell::<T, CellMutable>::new(initial);

        let weak = derived.downgrade();
        let first = Arc::new(AtomicBool::new(true));
        let guard = self.subscribe(move |result| {
            if first.swap(false, Ordering::SeqCst) {
                return;
            }
            if let Some(d) = weak.upgrade() {
                let value = match result {
                    Ok(v) => v.clone(),
                    Err(_) => default.clone(),
                };
                d.notify(value);
            }
        });
        derived.own(guard);

        // Propagate source completion
        let weak = derived.downgrade();
        let complete_guard = self.on_complete(move || {
            if let Some(d) = weak.upgrade() {
                d.complete();
            }
        });
        derived.own(complete_guard);

        derived.lock()
    }

    /// Unwrap Ok values, computing a fallback for Err.
    fn unwrap_or_else<F>(&self, f: F) -> Cell<T, CellImmutable>
    where
        T: Clone + Send + Sync + 'static,
        E: 'static,
        F: Fn(&E) -> T + Send + Sync + 'static,
        Self: Clone + Send + Sync + 'static,
    {
        let initial = match &self.get() {
            Ok(v) => v.clone(),
            Err(e) => f(e),
        };
        let derived = Cell::<T, CellMutable>::new(initial);

        let weak = derived.downgrade();
        let first = Arc::new(AtomicBool::new(true));
        let guard = self.subscribe(move |result| {
            if first.swap(false, Ordering::SeqCst) {
                return;
            }
            if let Some(d) = weak.upgrade() {
                let value = match result {
                    Ok(v) => v.clone(),
                    Err(e) => f(e),
                };
                d.notify(value);
            }
        });
        derived.own(guard);

        // Propagate source completion
        let weak = derived.downgrade();
        let complete_guard = self.on_complete(move || {
            if let Some(d) = weak.upgrade() {
                d.complete();
            }
        });
        derived.own(complete_guard);

        derived.lock()
    }
}

impl<T, E, W> UnwrapOrExt<T, E> for W
where
    T: Clone + Send + Sync + 'static,
    E: Clone + Send + Sync + 'static,
    W: Watchable<Result<T, E>>,
{}
