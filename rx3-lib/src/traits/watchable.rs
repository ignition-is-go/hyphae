use uuid::Uuid;
use super::DepNode;

/// Core reactive cell trait.
pub trait Watchable<T>: Clone + DepNode + Sized + Send + Sync + 'static {
    fn watch(&self, callback: impl Fn(&T) + Send + Sync + 'static) -> Uuid;
    fn unsubscribe(&self, id: Uuid);
    fn get(&self) -> T;
}
