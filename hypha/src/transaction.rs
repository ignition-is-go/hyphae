//! Transaction-based glitch-free propagation for reactive cells.
//!
//! This module implements a barrier-based synchronization mechanism that ensures
//! derived cells only fire once per transaction, even when they have multiple
//! paths to a common source (the "diamond problem").
//!
//! ## Mechanism
//!
//! 1. **TxId propagation**: When a source cell is set with a transaction,
//!    the TxId propagates downstream to all derived cells.
//!
//! 2. **Expected counts**: Each derived cell tracks how many updates to expect
//!    per source for each transaction.
//!
//! 3. **Barrier synchronization**: A cell only fires when it has received
//!    all expected updates for the transaction (`received == expected`).
//!
//! 4. **Cycle handling**: Cycles are detected during propagation. When detected,
//!    the cycle is logged and the update is interrupted to prevent infinite loops.
//!
//! ## Example: Diamond Problem
//!
//! ```text
//!      A (source)
//!     / \
//!    B   C  (derived from A)
//!     \ /
//!      D    (derived from B and C)
//! ```
//!
//! Without transactions: Setting A causes D to fire twice.
//! With transactions: D waits for both B and C updates, then fires once.

use dashmap::DashMap;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use tracing::{trace, warn};
use uuid::Uuid;

// ============================================================================
// Transaction Finalization Registry
// ============================================================================

/// A pending barrier that should fire when the transaction finalizes.
/// This is used to handle cases where a join/join_vec has sources that
/// don't all participate in the same transaction (e.g., independent sources).
pub struct PendingBarrier {
    /// Callback to fire the barrier with current values.
    pub fire_fn: Box<dyn FnOnce() + Send>,
}

// Thread-local registry for pending barriers within a transaction.
// When set_tx completes, all pending barriers for that TxId are finalized.
thread_local! {
    static PENDING_BARRIERS: RefCell<HashMap<TxId, Vec<PendingBarrier>>> = RefCell::new(HashMap::new());
}

/// Register a pending barrier for finalization when the transaction completes.
pub fn register_pending_barrier(txid: TxId, barrier: PendingBarrier) {
    PENDING_BARRIERS.with(|barriers| {
        barriers.borrow_mut().entry(txid).or_default().push(barrier);
    });
}

/// Finalize all pending barriers for a transaction.
/// Called by set_tx after synchronous propagation completes.
pub fn finalize_transaction(txid: TxId) {
    let barriers = PENDING_BARRIERS.with(|barriers| barriers.borrow_mut().remove(&txid));

    if let Some(pending) = barriers {
        trace!(
            txid = %txid,
            count = pending.len(),
            "finalizing transaction, firing pending barriers"
        );
        for barrier in pending {
            (barrier.fire_fn)();
        }
    }
}

/// Clear a specific pending barrier for a transaction (when barrier is met normally).
/// This prevents double-firing when the barrier completes before transaction finalization.
pub fn clear_pending_barrier(txid: TxId, index: usize) {
    PENDING_BARRIERS.with(|barriers| {
        if let Some(pending) = barriers.borrow_mut().get_mut(&txid)
            && index < pending.len() {
                // Replace with a no-op barrier to maintain indices
                pending[index] = PendingBarrier {
                    fire_fn: Box::new(|| {}),
                };
            }
    });
}

// ============================================================================
// Transaction Types
// ============================================================================

/// A unique identifier for a transaction.
///
/// Transactions are created when a source cell is set with transaction semantics.
/// The TxId propagates downstream through all derived cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TxId(pub(crate) u64);

impl TxId {
    /// Generate a new unique transaction ID.
    pub fn new() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(1);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        trace!(txid = id, "generated new transaction");
        TxId(id)
    }
}

impl Default for TxId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for TxId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "tx:{}", self.0)
    }
}

/// Unique identifier for a cell in the reactive graph.
pub type CellId = Uuid;

/// Tracks pending updates for a specific transaction within a cell.
///
/// Each cell maintains a map of `TxId -> PendingTx` to track how many
/// updates are expected from each source and how many have been received.
pub struct PendingTx {
    /// Expected update count per source cell.
    /// When a transaction propagates, we increment the expected count for each source.
    pub expected: DashMap<CellId, usize>,

    /// Total received updates for this transaction.
    /// When this equals the sum of all expected counts, the barrier is met.
    pub received: AtomicUsize,

    /// The latest values received from each source, stored for deferred notification.
    /// We store the signal's value bytes since we can't store generic Signal<T>.
    pub values: DashMap<CellId, ()>,
}

impl PendingTx {
    /// Create a new pending transaction tracker.
    pub fn new() -> Self {
        Self {
            expected: DashMap::new(),
            received: AtomicUsize::new(0),
            values: DashMap::new(),
        }
    }

    /// Get the total expected update count (sum of all per-source counts).
    pub fn total_expected(&self) -> usize {
        self.expected.iter().map(|entry| *entry.value()).sum()
    }

    /// Increment the expected count for a source.
    /// Returns the new expected count for that source.
    pub fn expect_from(&self, source_id: CellId, cell_id: CellId) -> usize {
        let new_count = self
            .expected
            .entry(source_id)
            .and_modify(|c| *c += 1)
            .or_insert(1);
        let count = *new_count;
        trace!(
            cell = %cell_id,
            source = %source_id,
            expected = count,
            "increment expected count"
        );
        count
    }

    /// Decrement the expected count for a source (used on unsubscribe).
    /// Returns the new expected count for that source.
    pub fn unexpect_from(&self, source_id: CellId, cell_id: CellId) -> usize {
        if let Some(mut entry) = self.expected.get_mut(&source_id)
            && *entry > 0 {
                *entry -= 1;
                let count = *entry;
                trace!(
                    cell = %cell_id,
                    source = %source_id,
                    expected = count,
                    "decrement expected count on unsubscribe"
                );
                return count;
            }
        0
    }

    /// Record that we received an update. Returns (received, total_expected).
    pub fn receive(&self, cell_id: CellId, txid: TxId) -> (usize, usize) {
        let received = self.received.fetch_add(1, Ordering::SeqCst) + 1;
        let expected = self.total_expected();
        trace!(
            cell = %cell_id,
            txid = %txid,
            received = received,
            expected = expected,
            "received update"
        );
        (received, expected)
    }

    /// Check if the barrier is met (all expected updates received).
    pub fn barrier_met(&self) -> bool {
        self.received.load(Ordering::SeqCst) >= self.total_expected()
    }
}

impl Default for PendingTx {
    fn default() -> Self {
        Self::new()
    }
}

/// Context for a transaction, passed through the reactive graph.
#[derive(Debug, Clone)]
pub struct TxContext {
    /// The transaction identifier.
    pub txid: TxId,

    /// Path of cell IDs visited so far (for cycle detection).
    /// Uses a compact representation to avoid allocation per notification.
    pub path: Arc<Vec<CellId>>,
}

impl TxContext {
    /// Create a new transaction context originating from a cell.
    pub fn new(origin: CellId) -> Self {
        let txid = TxId::new();
        trace!(
            txid = %txid,
            origin = %origin,
            "created transaction context"
        );
        Self {
            txid,
            path: Arc::new(vec![origin]),
        }
    }

    /// Create a new context with the current cell added to the path.
    /// Returns None if adding this cell would create a cycle.
    pub fn push(&self, cell_id: CellId) -> Option<Self> {
        if self.path.contains(&cell_id) {
            // Cycle detected!
            let cycle_path: Vec<_> = self
                .path
                .iter()
                .skip_while(|&&id| id != cell_id)
                .chain(std::iter::once(&cell_id))
                .map(|id| id.to_string())
                .collect();

            warn!(
                txid = %self.txid,
                cycle = ?cycle_path,
                "cycle detected in reactive graph"
            );
            return None;
        }

        let mut new_path = (*self.path).clone();
        new_path.push(cell_id);
        Some(Self {
            txid: self.txid,
            path: Arc::new(new_path),
        })
    }

    /// Check if visiting a cell would create a cycle.
    pub fn would_cycle(&self, cell_id: CellId) -> bool {
        self.path.contains(&cell_id)
    }
}

/// Manages transaction state for a cell.
///
/// This is stored in each cell's inner structure and tracks pending
/// transactions, managing the barrier synchronization.
pub struct TxState {
    /// Pending transactions for this cell.
    /// When a transaction arrives, we track how many updates to expect.
    pub pending: DashMap<TxId, PendingTx>,

    /// Set of source cells that were already updated in the current transaction.
    /// Used to prevent double-counting when multiple paths lead from the same source.
    completed_sources: DashMap<TxId, HashSet<CellId>>,
}

impl TxState {
    /// Create a new transaction state tracker.
    pub fn new() -> Self {
        Self {
            pending: DashMap::new(),
            completed_sources: DashMap::new(),
        }
    }

    /// Get or create the pending transaction entry for a TxId.
    pub fn get_or_create(&self, txid: TxId) -> dashmap::mapref::one::RefMut<'_, TxId, PendingTx> {
        self.pending.entry(txid).or_default()
    }

    /// Expect an update from a source cell for a transaction.
    /// Returns the new expected count for that source.
    pub fn expect(&self, txid: TxId, source_id: CellId, cell_id: CellId) -> usize {
        let pending = self.get_or_create(txid);
        pending.expect_from(source_id, cell_id)
    }

    /// Record receiving an update for a transaction.
    /// Returns (received, expected, barrier_met).
    pub fn receive(
        &self,
        txid: TxId,
        _source_id: CellId,
        cell_id: CellId,
    ) -> (usize, usize, bool) {
        if let Some(pending) = self.pending.get(&txid) {
            let (received, expected) = pending.receive(cell_id, txid);
            let barrier_met = received >= expected;

            if barrier_met {
                trace!(
                    cell = %cell_id,
                    txid = %txid,
                    "barrier met, firing cell"
                );
            }

            (received, expected, barrier_met)
        } else {
            // No pending entry means this is the first update for this transaction,
            // or we're receiving without prior expectation setup (edge case).
            // Treat as immediate fire.
            (1, 1, true)
        }
    }

    /// Clean up a completed transaction.
    pub fn cleanup(&self, txid: TxId) {
        self.pending.remove(&txid);
        self.completed_sources.remove(&txid);
    }

    /// Mark a source as completed for a transaction (to avoid double-counting).
    pub fn mark_source_completed(&self, txid: TxId, source_id: CellId) -> bool {
        let mut entry = self
            .completed_sources
            .entry(txid)
            .or_default();
        entry.insert(source_id)
    }

    /// Check if a source is already completed for this transaction.
    pub fn is_source_completed(&self, txid: TxId, source_id: CellId) -> bool {
        self.completed_sources
            .get(&txid)
            .map(|s| s.contains(&source_id))
            .unwrap_or(false)
    }

    /// Handle subscribing to a new source mid-transaction.
    /// Inherits the pending state from the source.
    pub fn subscribe_mid_tx(
        &self,
        txid: TxId,
        source_id: CellId,
        cell_id: CellId,
        source_pending_count: usize,
    ) {
        trace!(
            cell = %cell_id,
            source = %source_id,
            txid = %txid,
            inheriting = source_pending_count,
            "subscribing mid-transaction, inheriting pending count"
        );

        if source_pending_count > 0 {
            let pending = self.get_or_create(txid);
            pending
                .expected
                .entry(source_id)
                .and_modify(|c| *c += source_pending_count)
                .or_insert(source_pending_count);
        }
    }

    /// Handle unsubscribing from a source mid-transaction.
    /// Decrements the expected count.
    pub fn unsubscribe_mid_tx(&self, txid: TxId, source_id: CellId, cell_id: CellId) {
        if let Some(pending) = self.pending.get(&txid) {
            pending.unexpect_from(source_id, cell_id);
        }
    }
}

impl Default for TxState {
    fn default() -> Self {
        Self::new()
    }
}

/// Configuration for stale transaction detection.
pub struct StaleTxConfig {
    /// Timeout after which a pending transaction is considered stale.
    pub timeout_ms: u64,
    /// Callback invoked when a stale transaction is detected.
    pub on_stale: Option<Box<dyn Fn(TxId, CellId) + Send + Sync>>,
}

impl Default for StaleTxConfig {
    fn default() -> Self {
        Self {
            timeout_ms: 5000,
            on_stale: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_txid_generation() {
        let id1 = TxId::new();
        let id2 = TxId::new();
        assert_ne!(id1, id2);
    }

    #[test]
    fn test_pending_tx_barrier() {
        let pending = PendingTx::new();
        let cell_id = Uuid::new_v4();
        let source_a = Uuid::new_v4();
        let source_b = Uuid::new_v4();
        let txid = TxId::new();

        // Expect 1 update each from A and B
        pending.expect_from(source_a, cell_id);
        pending.expect_from(source_b, cell_id);

        assert!(!pending.barrier_met());

        // Receive from A
        pending.receive(cell_id, txid);
        assert!(!pending.barrier_met());

        // Receive from B - barrier should be met
        pending.receive(cell_id, txid);
        assert!(pending.barrier_met());
    }

    #[test]
    fn test_tx_context_cycle_detection() {
        let cell_a = Uuid::new_v4();
        let cell_b = Uuid::new_v4();
        let cell_c = Uuid::new_v4();

        let ctx = TxContext::new(cell_a);
        assert!(!ctx.would_cycle(cell_b));

        let ctx = ctx.push(cell_b).unwrap();
        assert!(!ctx.would_cycle(cell_c));
        assert!(ctx.would_cycle(cell_a)); // Would cycle back to start
        assert!(ctx.would_cycle(cell_b)); // Would cycle back to B

        let result = ctx.push(cell_a);
        assert!(result.is_none()); // Cycle detected, returns None
    }
}
