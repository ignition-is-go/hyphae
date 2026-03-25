use std::sync::Arc;

use uuid::Uuid;

use crate::traits::DepNode;

/// RAII guard that unsubscribes when dropped.
#[must_use = "subscription will be cancelled immediately if the guard is dropped"]
pub struct SubscriptionGuard {
    unsubscribe_fn: Option<Box<dyn FnMut() + Send + Sync>>,
    id: Uuid,
    source: Arc<dyn DepNode>,
}

/// Minimal DepNode for callback-only guards (no real cell dependency).
struct CallbackDepNode(Uuid);

impl DepNode for CallbackDepNode {
    fn id(&self) -> Uuid {
        self.0
    }
    fn name(&self) -> Option<String> {
        Some("callback_guard".to_string())
    }
    fn deps(&self) -> Vec<Arc<dyn DepNode>> {
        vec![]
    }
}

impl SubscriptionGuard {
    pub(crate) fn new(
        id: Uuid,
        source: Arc<dyn DepNode>,
        unsubscribe_fn: impl FnMut() + Send + Sync + 'static,
    ) -> Self {
        Self {
            unsubscribe_fn: Some(Box::new(unsubscribe_fn)),
            id,
            source,
        }
    }

    /// Create a guard that runs a callback when dropped.
    ///
    /// Unlike `new`, this does not require a real cell source — useful for
    /// cleanup actions (e.g., sending unsubscribe messages) that should be
    /// tied to a cell's lifetime via `cell.own()`.
    pub fn from_callback(callback: impl FnMut() + Send + Sync + 'static) -> Self {
        let id = Uuid::new_v4();
        log::trace!("SubscriptionGuard::from_callback created id={}", id);
        Self {
            unsubscribe_fn: Some(Box::new(callback)),
            id,
            source: Arc::new(CallbackDepNode(id)),
        }
    }

    /// Get the source cell this subscription is connected to.
    pub fn source(&self) -> &Arc<dyn DepNode> {
        &self.source
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
            log::trace!("SubscriptionGuard dropped id={} — running cleanup", self.id);
            f();
        }
    }
}
