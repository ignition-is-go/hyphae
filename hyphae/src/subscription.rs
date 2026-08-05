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

/// Minimal `DepNode` for callback-only guards (no real cell dependency).
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

/// Dependency node representing one installed pipeline with multiple roots.
///
/// The node is deliberately transparent to graph traversal: its dependencies
/// are the real subscription sources, and scheduler invalidation registration
/// is forwarded to each of them.
struct CompositeDepNode {
    id: Uuid,
    sources: Vec<Arc<dyn DepNode>>,
}

impl DepNode for CompositeDepNode {
    fn id(&self) -> Uuid {
        self.id
    }

    fn name(&self) -> Option<String> {
        Some("composite_subscription".to_string())
    }

    fn deps(&self) -> Vec<Arc<dyn DepNode>> {
        self.sources.clone()
    }

    #[cfg(feature = "scheduler")]
    fn add_height_dependent(&self, dep: std::sync::Weak<dyn crate::cell::HeightInvalidate>) {
        for source in &self.sources {
            source.add_height_dependent(dep.clone());
        }
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
        log::trace!("SubscriptionGuard::from_callback created id={id}");
        Self {
            unsubscribe_fn: Some(Box::new(callback)),
            id,
            source: Arc::new(CallbackDepNode(id)),
        }
    }

    /// Attach cleanup to this guard without changing its dependency source.
    ///
    /// Pipeline operators use this to stop auxiliary work such as interval
    /// timers when the materialized output drops. Keeping the original source
    /// is important: scheduler height calculation must still see the real
    /// upstream dependency rather than a callback-only placeholder.
    pub(crate) fn with_cleanup(self, mut cleanup: impl FnMut() + Send + Sync + 'static) -> Self {
        let source = self.source.clone();
        let mut guard = Some(self);
        Self::new(Uuid::new_v4(), source, move || {
            cleanup();
            if let Some(guard) = guard.take() {
                drop(guard);
            }
        })
    }

    /// Combine multiple upstream subscriptions into one pipeline guard.
    ///
    /// Dropping the returned guard drops every child guard. Its dependency
    /// source exposes every child source so materialized cells retain correct
    /// scheduler height and dependency-tree information.
    pub(crate) fn combine(guards: Vec<Self>) -> Self {
        match guards.len() {
            0 => Self::from_callback(|| {}),
            1 => {
                let mut guards = guards;
                guards.remove(0)
            }
            _ => {
                let id = Uuid::new_v4();
                let sources = guards.iter().map(|guard| guard.source.clone()).collect();
                let source = Arc::new(CompositeDepNode { id, sources });
                let mut guards = Some(guards);
                Self::new(id, source, move || {
                    if let Some(guards) = guards.take() {
                        drop(guards);
                    }
                })
            }
        }
    }

    /// Get the source cell this subscription is connected to.
    #[must_use]
    pub fn source(&self) -> &Arc<dyn DepNode> {
        &self.source
    }

    /// Prevent automatic unsubscribe on drop.
    /// Returns the subscription ID for manual management.
    #[must_use]
    pub fn leak(mut self) -> Uuid {
        self.unsubscribe_fn = None;
        self.id
    }

    /// Manually unsubscribe (same as dropping).
    pub fn unsubscribe(self) {
        // Just drop - Drop impl handles it
    }

    /// Get the subscription ID.
    #[must_use]
    pub const fn id(&self) -> Uuid {
        self.id
    }
}

impl Drop for SubscriptionGuard {
    fn drop(&mut self) {
        if let Some(mut f) = self.unsubscribe_fn.take() {
            log::trace!("SubscriptionGuard dropped id={} — running cleanup", self.id);
            f();
            // No scheduler height invalidation here: the only edge removal that
            // changes a *living* cell's height is an `own_keyed` replacement, and
            // that path re-invalidates the owner's height cone itself. A guard
            // dropping because its owner cell is being torn down needs no
            // invalidation (a cell with live height-dependents is kept alive by
            // their guards' source `Arc`s, so it can't drop out from under them),
            // and a loosely-held (non-cell-owned) guard has no dependent height to
            // invalidate. See `Cell::own`/`own_keyed` for the localized bump.
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    #[test]
    fn combined_guard_exposes_all_sources_and_drops_all_children() {
        let dropped = Arc::new(AtomicUsize::new(0));
        let source_a: Arc<dyn DepNode> = Arc::new(CallbackDepNode(Uuid::new_v4()));
        let source_b: Arc<dyn DepNode> = Arc::new(CallbackDepNode(Uuid::new_v4()));

        let guard_a = {
            let dropped = dropped.clone();
            SubscriptionGuard::new(Uuid::new_v4(), source_a.clone(), move || {
                dropped.fetch_add(1, Ordering::SeqCst);
            })
        };
        let guard_b = {
            let dropped = dropped.clone();
            SubscriptionGuard::new(Uuid::new_v4(), source_b.clone(), move || {
                dropped.fetch_add(1, Ordering::SeqCst);
            })
        };

        let guard = SubscriptionGuard::combine(vec![guard_a, guard_b]);
        let deps = guard.source().deps();
        assert_eq!(deps.len(), 2);
        assert_eq!(deps.first().map(|dep| dep.id()), Some(source_a.id()));
        assert_eq!(deps.get(1).map(|dep| dep.id()), Some(source_b.id()));

        drop(guard);
        assert_eq!(dropped.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn combining_one_guard_preserves_its_source() {
        let source: Arc<dyn DepNode> = Arc::new(CallbackDepNode(Uuid::new_v4()));
        let guard = SubscriptionGuard::new(Uuid::new_v4(), source.clone(), || {});
        let combined = SubscriptionGuard::combine(vec![guard]);
        assert_eq!(combined.source().id(), source.id());
    }
}
