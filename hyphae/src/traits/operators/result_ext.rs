//! Result-typed pipeline operators: map_ok, map_err, catch_error, unwrap_or.
//!
//! These wrap `MapExt::map` with fixed per-variant closures. They are pure
//! transformations and allocate no intermediate cells.

use super::{CellValue, MapExt, MapPipeline};
use crate::pipeline::Pipeline;

pub trait MapOkExt<T: CellValue, E: CellValue>: Pipeline<Result<T, E>> {
    #[track_caller]
    fn map_ok<U, F>(
        self,
        f: F,
    ) -> MapPipeline<
        Self,
        Result<T, E>,
        Result<U, E>,
        impl Fn(&Result<T, E>) -> Result<U, E> + Send + Sync + 'static,
    >
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

impl<T: CellValue, E: CellValue, P: Pipeline<Result<T, E>>> MapOkExt<T, E> for P {}

pub trait MapErrExt<T: CellValue, E: CellValue>: Pipeline<Result<T, E>> {
    #[track_caller]
    fn map_err<E2, F>(
        self,
        f: F,
    ) -> MapPipeline<
        Self,
        Result<T, E>,
        Result<T, E2>,
        impl Fn(&Result<T, E>) -> Result<T, E2> + Send + Sync + 'static,
    >
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

impl<T: CellValue, E: CellValue, P: Pipeline<Result<T, E>>> MapErrExt<T, E> for P {}

pub trait CatchErrorExt<T: CellValue, E: CellValue>: Pipeline<Result<T, E>> {
    #[track_caller]
    fn catch_error<F>(
        self,
        f: F,
    ) -> MapPipeline<
        Self,
        Result<T, E>,
        T,
        impl Fn(&Result<T, E>) -> T + Send + Sync + 'static,
    >
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

impl<T: CellValue, E: CellValue, P: Pipeline<Result<T, E>>> CatchErrorExt<T, E> for P {}

pub trait UnwrapOrExt<T: CellValue, E: CellValue>: Pipeline<Result<T, E>> {
    #[track_caller]
    fn unwrap_or(
        self,
        default: T,
    ) -> MapPipeline<
        Self,
        Result<T, E>,
        T,
        impl Fn(&Result<T, E>) -> T + Send + Sync + 'static,
    >
    where
        Self: Sized,
    {
        self.map(move |r: &Result<T, E>| match r {
            Ok(v) => v.clone(),
            Err(_) => default.clone(),
        })
    }

    #[track_caller]
    fn unwrap_or_else<F>(
        self,
        f: F,
    ) -> MapPipeline<
        Self,
        Result<T, E>,
        T,
        impl Fn(&Result<T, E>) -> T + Send + Sync + 'static,
    >
    where
        Self: Sized,
        F: Fn(&E) -> T + Send + Sync + 'static,
    {
        self.catch_error(f)
    }
}

impl<T: CellValue, E: CellValue, P: Pipeline<Result<T, E>>> UnwrapOrExt<T, E> for P {}
