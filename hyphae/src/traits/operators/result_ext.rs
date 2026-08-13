//! Result-typed pipeline operators: `map_ok`, `map_err`, `catch_error`, `unwrap_or`.
//!
//! These wrap `MapExt::map` with fixed per-variant closures. They are pure
//! transformations and allocate no intermediate cells.

use super::{CellValue, MapExt};
use crate::pipeline::{Materialize, Pipeline, Seedness};

#[allow(private_bounds)]
pub trait MapOkExt<T: CellValue, E: CellValue, S: Seedness>:
    Pipeline<Result<T, E>, S> + MapExt<Result<T, E>, S>
{
    #[track_caller]
    fn map_ok<U, F>(self, f: F) -> impl Materialize<Result<U, E>, S>
    where
        Self: Sized,
        U: CellValue,
        F: Fn(&T) -> U + Send + Sync + 'static,
    {
        self.map(move |r: &Result<T, E>| match r {
            Ok(v) => Ok(f(v)),
            Err(e) => Err(e.clone()),
        })
    }
}

impl<T: CellValue, E: CellValue, S: Seedness, P> MapOkExt<T, E, S> for P where
    P: Pipeline<Result<T, E>, S> + MapExt<Result<T, E>, S>
{
}

#[allow(private_bounds)]
pub trait MapErrExt<T: CellValue, E: CellValue, S: Seedness>:
    Pipeline<Result<T, E>, S> + MapExt<Result<T, E>, S>
{
    #[track_caller]
    fn map_err<E2, F>(self, f: F) -> impl Materialize<Result<T, E2>, S>
    where
        Self: Sized,
        E2: CellValue,
        F: Fn(&E) -> E2 + Send + Sync + 'static,
    {
        self.map(move |r: &Result<T, E>| match r {
            Ok(v) => Ok(v.clone()),
            Err(e) => Err(f(e)),
        })
    }
}

impl<T: CellValue, E: CellValue, S: Seedness, P> MapErrExt<T, E, S> for P where
    P: Pipeline<Result<T, E>, S> + MapExt<Result<T, E>, S>
{
}

#[allow(private_bounds)]
pub trait CatchErrorExt<T: CellValue, E: CellValue, S: Seedness>:
    Pipeline<Result<T, E>, S> + MapExt<Result<T, E>, S>
{
    #[track_caller]
    fn catch_error<F>(self, f: F) -> impl Materialize<T, S>
    where
        Self: Sized,
        F: Fn(&E) -> T + Send + Sync + 'static,
    {
        self.map(move |r: &Result<T, E>| match r {
            Ok(v) => v.clone(),
            Err(e) => f(e),
        })
    }
}

impl<T: CellValue, E: CellValue, S: Seedness, P> CatchErrorExt<T, E, S> for P where
    P: Pipeline<Result<T, E>, S> + MapExt<Result<T, E>, S>
{
}

#[allow(private_bounds)]
pub trait UnwrapOrExt<T: CellValue, E: CellValue, S: Seedness>:
    Pipeline<Result<T, E>, S> + MapExt<Result<T, E>, S> + CatchErrorExt<T, E, S>
{
    #[track_caller]
    fn unwrap_or(self, default: T) -> impl Materialize<T, S>
    where
        Self: Sized,
    {
        self.map(move |r: &Result<T, E>| {
            r.as_ref()
                .map_or_else(|_| default.clone(), std::clone::Clone::clone)
        })
    }

    #[track_caller]
    fn unwrap_or_else<F>(self, f: F) -> impl Materialize<T, S>
    where
        Self: Sized,
        F: Fn(&E) -> T + Send + Sync + 'static,
    {
        self.catch_error(f)
    }
}

impl<T: CellValue, E: CellValue, S: Seedness, P> UnwrapOrExt<T, E, S> for P where
    P: Pipeline<Result<T, E>, S> + MapExt<Result<T, E>, S> + CatchErrorExt<T, E, S>
{
}
