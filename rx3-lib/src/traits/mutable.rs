use super::Watchable;

pub trait Mutable<T>: Watchable<T> {
    fn set(&self, value: T);

    /// Mark this cell as complete. No more values should be emitted after this.
    fn complete(&self);

    /// Mark this cell as errored. No more values will be emitted after this.
    fn fail(&self, error: impl Into<anyhow::Error>);
}
