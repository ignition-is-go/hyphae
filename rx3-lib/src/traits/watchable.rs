use super::DepNode;

/// Core reactive cell trait.
pub trait Watchable<T>: Clone + DepNode + Sized + Send + Sync + 'static {
    fn watch(&self, callback: impl Fn(&T) + Send + Sync + 'static);
    fn get(&self) -> T;
}
