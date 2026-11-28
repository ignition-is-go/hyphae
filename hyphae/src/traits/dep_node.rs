use std::sync::Arc;

use uuid::Uuid;

pub mod sealed {
    /// Only hyphae's own nodes take part in propagation.
    pub trait Sealed {}
}

/// The type-erased edge the scheduler propagates along: a node's identity,
/// height and the nodes it depends on.
///
/// Sealed: only hyphae's cells, sources and subscription guards implement it.
#[doc(hidden)]
pub trait DepNode: sealed::Sealed + Send + Sync {
    fn id(&self) -> Uuid;
    /// Returns the dependencies of this node.
    fn deps(&self) -> Vec<Arc<dyn DepNode>>;

    /// Propagation height: strictly greater than the height of every node
    /// this one depends on. Cells keep it as a maintained atomic (see
    /// `crate::cell::raise_height`); other nodes derive it from `deps()`.
    #[doc(hidden)]
    fn height(&self) -> u64 {
        self.deps()
            .iter()
            .map(|dep| dep.height().saturating_add(1))
            .max()
            .unwrap_or(0)
    }

    /// Register `dep` as a node whose height must stay above this one's, so a
    /// later raise of this node's height also raises `dep`. Called when a cell
    /// takes ownership of a subscription guard whose source is this node.
    /// Cells push into `CellInner::height_dependents`; other nodes ignore it.
    fn add_height_dependent(&self, _dep: std::sync::Weak<dyn crate::cell::HeightNode>) {}

    /// Whether the scheduler should skip last-write-wins coalescing for this
    /// node — enqueuing every notify as a distinct height-ordered op so event
    /// semantics (scan/pairwise/merge and hand-rolled stateful maps) survive a
    /// batch. Cells expose their per-cell flag; other nodes default to `false`.
    fn no_coalesce(&self) -> bool {
        false
    }

    /// Check that a cell of scheduling domain `reader` may depend on this node.
    /// `Ok(true)` means the dependency crosses the boundary from the root
    /// domain into a child. Nodes that are not cells belong to no domain and
    /// accept every reader.
    ///
    /// # Errors
    ///
    /// Returns the [`DomainError`](crate::scheduler::DomainError) describing a
    /// dependency that points from root to a child or between two children.
    fn check_reader(&self, _reader: u64) -> Result<bool, crate::scheduler::DomainError> {
        Ok(false)
    }

    /// Returns the number of active subscribers to this node.
    #[doc(hidden)]
    fn subscriber_count(&self) -> usize {
        0
    }

    /// Returns the number of subscription guards owned by this node.
    #[doc(hidden)]
    fn owned_count(&self) -> usize {
        0
    }
}
