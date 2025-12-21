use uuid::Uuid;
use super::DepNode;

/// Read the current value from a reactive cell.
pub trait Gettable<T> {
    fn get(&self) -> T;
}

/// Core reactive cell trait.
///
/// Note: Prefer using `SubscribeExt::subscribe()` which returns an RAII guard
/// that automatically unsubscribes when dropped.
pub trait Watchable<T>: Clone + Gettable<T> + DepNode + Sized + Send + Sync + 'static {
    fn watch(&self, callback: impl Fn(&T) + Send + Sync + 'static) -> Uuid;
    fn unsubscribe(&self, id: Uuid);
}
