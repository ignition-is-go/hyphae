use super::Watchable;

pub trait Mutable<T>: Watchable<T> {
    fn set(&self, value: T);
}
