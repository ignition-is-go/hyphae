use uuid::Uuid;

/// RAII guard that unsubscribes when dropped.
#[must_use = "subscription will be cancelled immediately if the guard is dropped"]
pub struct SubscriptionGuard {
    unsubscribe_fn: Option<Box<dyn FnMut() + Send + Sync>>,
    id: Uuid,
}

impl SubscriptionGuard {
    pub(crate) fn new(id: Uuid, unsubscribe_fn: impl FnMut() + Send + Sync + 'static) -> Self {
        Self {
            unsubscribe_fn: Some(Box::new(unsubscribe_fn)),
            id,
        }
    }

    /// Prevent automatic unsubscribe on drop.
    /// Returns the subscription ID for manual management.
    pub fn leak(mut self) -> Uuid {
        self.unsubscribe_fn = None;
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

impl Drop for SubscriptionGuard {
    fn drop(&mut self) {
        if let Some(mut f) = self.unsubscribe_fn.take() {
            f();
        }
    }
}
