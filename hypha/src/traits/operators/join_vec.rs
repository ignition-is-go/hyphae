use dashmap::DashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use tracing::trace;

use crate::cell::{Cell, CellImmutable, CellMutable};
use crate::signal::Signal;
use crate::traits::Mutable;
use crate::transaction::{register_pending_barrier, PendingBarrier, TxContext, TxId};

use super::Watchable;

/// Tracks pending transaction state for a join_vec operation.
/// For join_vec with N sources, we expect N updates before firing.
/// If not all sources participate, finalization fires with available + current values.
struct JoinVecTxState<T> {
    /// Number of sources
    num_sources: usize,
    /// Received values per transaction.
    /// Key: TxId, Value: (vec of received values indexed by source, received count, fired flag)
    pending: DashMap<TxId, JoinVecTxEntry<T>>,
}

struct JoinVecTxEntry<T> {
    values: Vec<Option<Arc<T>>>,
    count: AtomicUsize,
    /// Flag to prevent double-firing (barrier met vs finalization)
    fired: AtomicBool,
}

impl<T> JoinVecTxEntry<T> {
    fn new(num_sources: usize) -> Self {
        Self {
            values: vec![None; num_sources],
            count: AtomicUsize::new(0),
            fired: AtomicBool::new(false),
        }
    }
}

impl<T: Clone> JoinVecTxState<T> {
    fn new(num_sources: usize) -> Self {
        Self {
            num_sources,
            pending: DashMap::new(),
        }
    }

    /// Record a value from source at index.
    /// Returns Some((values, ctx)) if barrier is met (all sources reported).
    fn receive(
        &self,
        txid: TxId,
        source_idx: usize,
        value: Arc<T>,
        ctx: &TxContext,
    ) -> Option<(Vec<T>, TxContext)> {
        let num_sources = self.num_sources;
        let mut entry = self.pending.entry(txid).or_insert_with(|| {
            JoinVecTxEntry::new(num_sources)
        });

        // Only count this source once per transaction
        if entry.values[source_idx].is_none() {
            entry.values[source_idx] = Some(value);
            let count = entry.count.fetch_add(1, Ordering::SeqCst) + 1;

            if count >= self.num_sources && !entry.fired.swap(true, Ordering::SeqCst) {
                // All sources have reported and we haven't fired yet
                let values: Vec<T> = entry
                    .values
                    .iter()
                    .filter_map(|v| v.as_ref().map(|arc| (**arc).clone()))
                    .collect();

                if values.len() == self.num_sources {
                    // Remove the entry after firing
                    drop(entry);
                    self.pending.remove(&txid);
                    return Some((values, ctx.clone()));
                }
            }
        }
        None
    }

    /// Mark this transaction as fired (called when finalization fires the barrier).
    fn mark_fired(&self, txid: TxId) -> bool {
        if let Some(entry) = self.pending.get(&txid) {
            !entry.fired.swap(true, Ordering::SeqCst)
        } else {
            false
        }
    }

    /// Get the stored values for a transaction (for finalization).
    fn get_values(&self, txid: TxId) -> Vec<Option<Arc<T>>> {
        if let Some(entry) = self.pending.get(&txid) {
            entry.values.clone()
        } else {
            vec![]
        }
    }

    /// Remove a completed transaction entry.
    fn cleanup(&self, txid: TxId) {
        self.pending.remove(&txid);
    }
}

/// Combines a vector of cells into a single cell that emits `Vec<T>`.
///
/// The resulting cell emits whenever any input cell changes.
/// Completes when all input cells complete.
/// Errors immediately if any input cell errors.
///
/// With transaction context, it waits for all sources to report for the same
/// transaction before firing once, eliminating glitches.
///
/// # Example
/// ```
/// use hypha::{Cell, Mutable, Gettable, join_vec};
///
/// let a = Cell::new(1);
/// let b = Cell::new(2);
/// let c = Cell::new(3);
///
/// let combined = join_vec(vec![a.clone().lock(), b.lock(), c.lock()]);
/// assert_eq!(combined.get(), vec![1, 2, 3]);
///
/// a.set(10);
/// assert_eq!(combined.get(), vec![10, 2, 3]);
/// ```
pub fn join_vec<T, W>(cells: Vec<W>) -> Cell<Vec<T>, CellImmutable>
where
    T: Clone + Send + Sync + 'static,
    W: Watchable<T> + Clone + Send + Sync + 'static,
{
    if cells.is_empty() {
        let derived = Cell::<Vec<T>, CellMutable>::new(vec![]);
        derived.complete();
        return derived.lock();
    }

    // Get initial values
    let initial: Vec<T> = cells.iter().map(|c| c.get()).collect();
    let derived = Cell::<Vec<T>, CellMutable>::new(initial);
    let derived_id = derived.id();

    let num_cells = cells.len();
    let complete_count = Arc::new(AtomicUsize::new(0));
    let cells = Arc::new(cells);
    let tx_state = Arc::new(JoinVecTxState::<T>::new(num_cells));

    // Subscribe to each cell
    for i in 0..num_cells {
        let weak = derived.downgrade();
        let cells_clone = cells.clone();
        let first = Arc::new(AtomicBool::new(true));
        let cc = complete_count.clone();
        let nc = num_cells;
        let tx_state_clone = tx_state.clone();
        let source_idx = i;

        let guard = cells[i].subscribe(move |signal| {
            if let Some(d) = weak.upgrade() {
                match signal {
                    Signal::Value(value, ctx) => {
                        // Skip first emission (initial value already set)
                        if first.swap(false, Ordering::SeqCst) {
                            return;
                        }

                        // Check for transaction context
                        if let Some(ctx) = ctx {
                            // Transaction mode: wait for barrier
                            trace!(
                                cell = %derived_id,
                                txid = %ctx.txid,
                                source_idx = source_idx,
                                "join_vec received value with transaction"
                            );

                            // Check for cycle
                            if ctx.would_cycle(derived_id) {
                                trace!(
                                    cell = %derived_id,
                                    txid = %ctx.txid,
                                    "cycle detected, skipping"
                                );
                                return;
                            }

                            let txid = ctx.txid;
                            // Try to complete the barrier
                            if let Some((values, ctx)) =
                                tx_state_clone.receive(ctx.txid, source_idx, value.clone(), ctx)
                            {
                                // Barrier met! Push the context forward
                                if let Some(new_ctx) = ctx.push(derived_id) {
                                    trace!(
                                        cell = %derived_id,
                                        txid = %new_ctx.txid,
                                        "join_vec barrier met, firing"
                                    );
                                    d.notify(Signal::Value(Arc::new(values), Some(new_ctx)));
                                }
                            } else {
                                // Barrier not met - register a pending barrier for finalization
                                // This fires if not all sources participate in this transaction
                                let tx_state_fin = tx_state_clone.clone();
                                let d_fin = d.clone();
                                let ctx_fin = ctx.clone();
                                let cells_fin = cells_clone.clone();
                                let derived_id_fin = derived_id;
                                let num_cells_fin = nc;

                                register_pending_barrier(txid, PendingBarrier {
                                    fire_fn: Box::new(move || {
                                        // Check if we already fired (barrier met naturally)
                                        if tx_state_fin.mark_fired(txid) {
                                            let stored = tx_state_fin.get_values(txid);
                                            // Build values: use stored if present, else current value
                                            let values: Vec<T> = (0..num_cells_fin)
                                                .map(|i| {
                                                    stored.get(i)
                                                        .and_then(|v| v.as_ref())
                                                        .map(|arc| (**arc).clone())
                                                        .unwrap_or_else(|| cells_fin[i].get())
                                                })
                                                .collect();

                                            if let Some(new_ctx) = ctx_fin.push(derived_id_fin) {
                                                trace!(
                                                    cell = %derived_id_fin,
                                                    txid = %txid,
                                                    "join_vec finalization firing"
                                                );
                                                d_fin.notify(Signal::Value(Arc::new(values), Some(new_ctx)));
                                            }
                                            tx_state_fin.cleanup(txid);
                                        }
                                    }),
                                });
                            }
                        } else {
                            // Non-transaction mode: immediate fire
                            let values: Vec<T> = cells_clone.iter().map(|c| c.get()).collect();
                            d.notify(Signal::value(values));
                        }
                    }
                    Signal::Complete => {
                        let prev = cc.fetch_add(1, Ordering::SeqCst);
                        if prev + 1 == nc {
                            // All cells have completed
                            d.notify(Signal::Complete);
                        }
                    }
                    Signal::Error(e) => {
                        // Error from any cell propagates immediately
                        d.notify(Signal::Error(e.clone()));
                    }
                }
            }
        });
        derived.own(guard);
    }

    derived.lock()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::traits::Gettable;
    use crate::Mutable;

    #[test]
    fn test_join_vec_empty() {
        let combined: Cell<Vec<i32>, CellImmutable> = join_vec::<i32, Cell<i32, CellImmutable>>(vec![]);
        assert_eq!(combined.get(), Vec::<i32>::new());
        assert!(combined.is_complete());
    }

    #[test]
    fn test_join_vec_single() {
        let a = Cell::new(42);
        let a_locked = a.clone().lock();
        let combined = join_vec(vec![a_locked]);
        assert_eq!(combined.get(), vec![42]);

        a.set(100);
        assert_eq!(combined.get(), vec![100]);
    }

    #[test]
    fn test_join_vec_multiple() {
        let a = Cell::new(1);
        let b = Cell::new(2);
        let c = Cell::new(3);

        let combined = join_vec(vec![a.clone().lock(), b.clone().lock(), c.clone().lock()]);
        assert_eq!(combined.get(), vec![1, 2, 3]);

        a.set(10);
        assert_eq!(combined.get(), vec![10, 2, 3]);

        b.set(20);
        assert_eq!(combined.get(), vec![10, 20, 3]);

        c.set(30);
        assert_eq!(combined.get(), vec![10, 20, 30]);
    }

    #[test]
    fn test_join_vec_completion() {
        let a = Cell::new(1);
        let b = Cell::new(2);

        let combined = join_vec(vec![a.clone().lock(), b.clone().lock()]);
        assert!(!combined.is_complete());

        a.complete();
        assert!(!combined.is_complete());

        b.complete();
        assert!(combined.is_complete());
    }

    #[test]
    fn test_join_vec_subscription() {
        use std::sync::atomic::AtomicI32;

        let a = Cell::new(1);
        let b = Cell::new(2);

        let combined = join_vec(vec![a.clone().lock(), b.clone().lock()]);

        let count = Arc::new(AtomicI32::new(0));
        let count_clone = count.clone();

        let _guard = combined.subscribe(move |signal| {
            if let Signal::Value(_, _) = signal {
                count_clone.fetch_add(1, Ordering::SeqCst);
            }
        });

        // Initial subscription triggers once
        assert_eq!(count.load(Ordering::SeqCst), 1);

        a.set(10);
        assert_eq!(count.load(Ordering::SeqCst), 2);

        b.set(20);
        assert_eq!(count.load(Ordering::SeqCst), 3);
    }
}
