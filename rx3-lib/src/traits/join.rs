use crate::cell::{Cell, CellImmutable};
use super::Watchable;

pub trait JoinExt<T>: Watchable<T> {
    fn join<U, M>(&self, other: &Cell<U, M>) -> Cell<(T, U), CellImmutable>
    where
        T: Clone + Send + Sync + 'static,
        U: Clone + Send + Sync + 'static,
        M: Send + Sync + 'static,
    {
        crate::combine!((self, other), |a: &T, b: &U| (a.clone(), b.clone()))
    }
}

impl<T, W: Watchable<T>> JoinExt<T> for W {}
