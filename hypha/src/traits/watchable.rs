use std::sync::Arc;
use uuid::Uuid;
use super::DepNode;
use crate::signal::Signal;
use crate::subscription::SubscriptionGuard;

/// Read the current value from a reactive cell.
pub trait Gettable<T> {
    fn get(&self) -> T;
}

/// Core reactive cell trait - subscribe to changes.
pub trait Watchable<T>: Clone + Gettable<T> + DepNode + Sized + Send + Sync + 'static {
    /// Subscribe to all signals (values, completion, errors). Returns a guard that unsubscribes when dropped.
    fn subscribe(&self, callback: impl Fn(&Signal<T>) + Send + Sync + 'static) -> SubscriptionGuard;

    /// Unsubscribe by ID (for internal use).
    fn unsubscribe(&self, id: Uuid);

    /// Returns true if this cell has completed (no more values will be emitted).
    fn is_complete(&self) -> bool;

    /// Returns true if this cell has errored.
    fn is_error(&self) -> bool;

    /// Returns the error if this cell has errored.
    fn error(&self) -> Option<Arc<anyhow::Error>>;
}
