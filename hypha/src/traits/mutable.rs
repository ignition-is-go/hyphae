use super::Watchable;
use crate::transaction::TxId;

pub trait Mutable<T>: Watchable<T> {
    /// Set the cell's value without transaction semantics.
    fn set(&self, value: T);

    /// Set the cell's value with transaction semantics for glitch-free propagation.
    ///
    /// This creates a new transaction that propagates through all derived cells.
    /// Derived cells that depend on multiple paths from this source will wait
    /// until they receive updates from all paths before firing, eliminating glitches.
    ///
    /// Returns the transaction ID for tracing/debugging purposes.
    fn set_tx(&self, value: T) -> TxId;

    /// Mark this cell as complete. No more values should be emitted after this.
    fn complete(&self);

    /// Mark this cell as errored. No more values will be emitted after this.
    fn fail(&self, error: impl Into<anyhow::Error>);
}
