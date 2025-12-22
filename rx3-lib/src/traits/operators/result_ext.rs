use crate::cell::{Cell, CellImmutable};
use super::{MapExt, Watchable};

/// Extension trait for transforming Ok values in Result cells.
pub trait MapOkExt<T, E>: Watchable<Result<T, E>> {
    /// Transform the Ok value, passing through Err unchanged.
    /// Equivalent to `map(|r| r.as_ref().map(f).map_or_else(|e| Err(e.clone()), Ok))`.
    fn map_ok<U, F>(&self, f: F) -> Cell<Result<U, E>, CellImmutable>
    where
        T: Clone + 'static,
        U: Clone + Send + Sync + 'static,
        E: Clone + Send + Sync + 'static,
        F: Fn(&T) -> U + Send + Sync + 'static,
        Self: Clone + Send + Sync + 'static,
    {
        self.map(move |r| match r {
            Ok(v) => Ok(f(v)),
            Err(e) => Err(e.clone()),
        })
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
    /// Equivalent to `map(|r| r.clone().map_err(|e| f(&e)))`.
    fn map_err<E2, F>(&self, f: F) -> Cell<Result<T, E2>, CellImmutable>
    where
        T: Clone + Send + Sync + 'static,
        E: Clone + Send + Sync + 'static,
        E2: Clone + Send + Sync + 'static,
        F: Fn(&E) -> E2 + Send + Sync + 'static,
        Self: Clone + Send + Sync + 'static,
    {
        self.map(move |r| match r {
            Ok(v) => Ok(v.clone()),
            Err(e) => Err(f(e)),
        })
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
    /// Converts `Cell<Result<T, E>>` to `Cell<T>` by handling errors.
    fn catch_error<F>(&self, f: F) -> Cell<T, CellImmutable>
    where
        T: Clone + Send + Sync + 'static,
        E: Clone + Send + Sync + 'static,
        F: Fn(&E) -> T + Send + Sync + 'static,
        Self: Clone + Send + Sync + 'static,
    {
        self.map(move |r| match r {
            Ok(v) => v.clone(),
            Err(e) => f(e),
        })
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
        E: Clone + Send + Sync + 'static,
        Self: Clone + Send + Sync + 'static,
    {
        self.map(move |r| match r {
            Ok(v) => v.clone(),
            Err(_) => default.clone(),
        })
    }

    /// Unwrap Ok values, computing a fallback for Err.
    /// Equivalent to `catch_error(f)`.
    fn unwrap_or_else<F>(&self, f: F) -> Cell<T, CellImmutable>
    where
        T: Clone + Send + Sync + 'static,
        E: Clone + Send + Sync + 'static,
        F: Fn(&E) -> T + Send + Sync + 'static,
        Self: Clone + Send + Sync + 'static,
    {
        self.catch_error(f)
    }
}

impl<T, E, W> UnwrapOrExt<T, E> for W
where
    T: Clone + Send + Sync + 'static,
    E: Clone + Send + Sync + 'static,
    W: Watchable<Result<T, E>>,
{}
