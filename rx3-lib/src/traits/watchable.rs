use uuid::Uuid;
use super::DepNode;
use crate::subscription::SubscriptionGuard;

/// Read the current value from a reactive cell.
pub trait Gettable<T> {
    fn get(&self) -> T;
}

/// Core reactive cell trait - subscribe to changes.
pub trait Watchable<T>: Clone + Gettable<T> + DepNode + Sized + Send + Sync + 'static {
    /// Subscribe to changes. Returns a guard that unsubscribes when dropped.
    fn subscribe(&self, callback: impl Fn(&T) + Send + Sync + 'static) -> SubscriptionGuard;

    /// Unsubscribe by ID (for internal use).
    fn unsubscribe(&self, id: Uuid);

    /// Register a callback to be called when this cell completes.
    /// Returns a guard that unregisters the callback when dropped.
    fn on_complete(&self, callback: impl Fn() + Send + Sync + 'static) -> SubscriptionGuard;

    /// Returns true if this cell has completed (no more values will be emitted).
    fn is_complete(&self) -> bool;
}
