use std::sync::Arc;

use uuid::Uuid;

use super::DepNode;
use crate::{signal::Signal, subscription::SubscriptionGuard};

/// Read the current value from a reactive cell.
pub trait Gettable<T> {
    fn get(&self) -> T;
}

/// Core reactive cell trait - subscribe to changes.
pub trait Watchable<T>: Clone + Gettable<T> + DepNode + Sized + Send + Sync + 'static {
    /// Subscribe to all signals (values, completion, errors). Returns a guard that unsubscribes when dropped.
    ///
    /// The callback must not panic. `Cell::notify` walks subscribers in order and does not
    /// isolate panics — a panicking subscriber propagates out of the `set`/`send` call that
    /// triggered the notification and prevents later subscribers in the same fanout from
    /// observing the signal, leaving the reactive graph in a partially-updated state. Keep
    /// subscribers pure and fast; handle expected errors inside the callback.
    fn subscribe(&self, callback: impl Fn(&Signal<T>) + Send + Sync + 'static)
    -> SubscriptionGuard;

    /// Unsubscribe by ID (for internal use).
    fn unsubscribe(&self, id: Uuid);

    /// Returns true if this cell has completed (no more values will be emitted).
    fn is_complete(&self) -> bool;

    /// Returns true if this cell has errored.
    fn is_error(&self) -> bool;

    /// Returns the error if this cell has errored.
    fn error(&self) -> Option<Arc<anyhow::Error>>;
}

/// Variant of [`Watchable`] whose subscribers return `Result<(), String>`.
///
/// Use this when a subscriber performs IO or other operations with expected
/// failure modes — the explicit `Result` makes the error channel visible
/// instead of inviting `unwrap`/`panic!` inside the callback. `Err` values
/// are logged via `log::error!` and do not propagate or interrupt the fanout.
///
/// Fallible subscribers run after all infallible ([`Watchable::subscribe`])
/// subscribers in each notify, so an error in a result-subscriber cannot
/// affect the regular chain. The panic contract on [`Watchable::subscribe`]
/// still applies.
pub trait WatchableResult<T>: Watchable<T> {
    /// Subscribe with a fallible callback. See the trait docs for semantics.
    ///
    /// The callback is invoked synchronously with the cell's current value
    /// (and any prior `Complete`/`Error` signal) before this function returns,
    /// mirroring [`Watchable::subscribe`]. Returns a guard that unsubscribes
    /// when dropped.
    fn subscribe_result(
        &self,
        callback: impl Fn(&Signal<T>) -> Result<(), String> + Send + Sync + 'static,
    ) -> SubscriptionGuard;
}
