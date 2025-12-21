use super::Watchable;

pub trait Mutable<T>: Watchable<T> {
    fn set(&self, value: T);

    /// Mark this cell as complete. No more values should be emitted after this.
    /// Calls all registered on_complete callbacks.
    fn complete(&self);
}
