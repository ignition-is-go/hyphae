use uuid::Uuid;
use crate::traits::Watchable;

/// RAII guard that unsubscribes when dropped.
#[must_use = "subscription will be cancelled immediately if the guard is dropped"]
pub struct SubscriptionGuard<T, C>
where
    C: Watchable<T>,
{
    cell: C,
    id: Uuid,
    active: bool,
    // Use fn() -> T to be covariant without requiring T: Send + Sync
    _marker: std::marker::PhantomData<fn() -> T>,
}

impl<T, C> SubscriptionGuard<T, C>
where
    C: Watchable<T>,
{
    pub(crate) fn new(cell: C, id: Uuid) -> Self {
        Self {
            cell,
            id,
            active: true,
            _marker: std::marker::PhantomData,
        }
    }

    /// Prevent automatic unsubscribe on drop.
    /// Returns the subscription ID for manual management.
    pub fn leak(mut self) -> Uuid {
        self.active = false;
        self.id
    }

    /// Manually unsubscribe (same as dropping).
    pub fn unsubscribe(self) {
        // Just drop - Drop impl handles it
    }

    /// Get the subscription ID.
    pub fn id(&self) -> Uuid {
        self.id
    }
}

impl<T, C> Drop for SubscriptionGuard<T, C>
where
    C: Watchable<T>,
{
    fn drop(&mut self) {
        if self.active {
            self.cell.unsubscribe(self.id);
        }
    }
}
