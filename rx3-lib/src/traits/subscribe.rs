use crate::subscription::SubscriptionGuard;
use super::Watchable;

pub trait SubscribeExt<T>: Watchable<T> + Sized {
    /// Subscribe with RAII guard - automatically unsubscribes when guard is dropped.
    fn subscribe(&self, callback: impl Fn(&T) + Send + Sync + 'static) -> SubscriptionGuard<T, Self>
    where
        T: 'static,
    {
        let id = self.watch(callback);
        SubscriptionGuard::new(self.clone(), id)
    }
}

impl<T, W: Watchable<T>> SubscribeExt<T> for W {}
