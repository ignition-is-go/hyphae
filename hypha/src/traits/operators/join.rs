use dashmap::DashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use tracing::trace;

use crate::cell::{Cell, CellImmutable, CellMutable};
use crate::signal::Signal;
use crate::transaction::{register_pending_barrier, PendingBarrier, TxContext, TxId};
use super::{Gettable, Watchable};

// Completion state flags for join (both must complete)
const SELF_COMPLETE: u8 = 0b01;
const OTHER_COMPLETE: u8 = 0b10;

/// Tracks pending transaction state for a join operation.
/// For join, we expect updates from both sources before firing.
/// If only one source fires (independent sources), we register a pending barrier
/// that fires during transaction finalization.
struct JoinTxState<T, U> {
    /// Received values per source for each transaction.
    /// Key: TxId, Value: (Option<T>, Option<U>, received_count, fired_flag)
    pending: DashMap<TxId, JoinTxEntry<T, U>>,
}

struct JoinTxEntry<T, U> {
    left: Option<Arc<T>>,
    right: Option<Arc<U>>,
    count: AtomicUsize,
    /// Flag to prevent double-firing (barrier met vs finalization)
    fired: AtomicBool,
}

impl<T, U> JoinTxEntry<T, U> {
    fn new() -> Self {
        Self {
            left: None,
            right: None,
            count: AtomicUsize::new(0),
            fired: AtomicBool::new(false),
        }
    }
}

impl<T: Clone, U: Clone> JoinTxState<T, U> {
    fn new() -> Self {
        Self {
            pending: DashMap::new(),
        }
    }

    /// Record a value from source A (left side).
    /// Returns Some((left, right, ctx)) if barrier is met (both sources reported).
    fn receive_left(&self, txid: TxId, value: Arc<T>, ctx: &TxContext) -> Option<(Arc<T>, Arc<U>, TxContext)> {
        let mut entry = self.pending.entry(txid).or_insert_with(JoinTxEntry::new);

        entry.left = Some(value);
        let count = entry.count.fetch_add(1, Ordering::SeqCst) + 1;

        if count >= 2 && !entry.fired.swap(true, Ordering::SeqCst) {
            // Both sources have reported and we haven't fired yet
            if let (Some(left), Some(right)) = (entry.left.clone(), entry.right.clone()) {
                drop(entry);
                self.pending.remove(&txid);
                return Some((left, right, ctx.clone()));
            }
        }
        None
    }

    /// Record a value from source B (right side).
    /// Returns Some((left, right, ctx)) if barrier is met (both sources reported).
    fn receive_right(&self, txid: TxId, value: Arc<U>, ctx: &TxContext) -> Option<(Arc<T>, Arc<U>, TxContext)> {
        let mut entry = self.pending.entry(txid).or_insert_with(JoinTxEntry::new);

        entry.right = Some(value);
        let count = entry.count.fetch_add(1, Ordering::SeqCst) + 1;

        if count >= 2 && !entry.fired.swap(true, Ordering::SeqCst) {
            // Both sources have reported and we haven't fired yet
            if let (Some(left), Some(right)) = (entry.left.clone(), entry.right.clone()) {
                drop(entry);
                self.pending.remove(&txid);
                return Some((left, right, ctx.clone()));
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
    fn get_values(&self, txid: TxId) -> (Option<Arc<T>>, Option<Arc<U>>) {
        if let Some(entry) = self.pending.get(&txid) {
            (entry.left.clone(), entry.right.clone())
        } else {
            (None, None)
        }
    }

    /// Remove a completed transaction entry.
    fn cleanup(&self, txid: TxId) {
        self.pending.remove(&txid);
    }
}

pub trait JoinExt<T>: Watchable<T> {
    fn join<U, M>(&self, other: &Cell<U, M>) -> Cell<(T, U), CellImmutable>
    where
        T: Clone + Send + Sync + 'static,
        U: Clone + Send + Sync + 'static,
        M: Send + Sync + 'static,
        Self: Clone + Send + Sync + 'static,
    {
        let initial = (self.get(), other.get());
        let derived = Cell::<(T, U), CellMutable>::new(initial);
        let derived_id = derived.id();

        let complete_state = Arc::new(AtomicU8::new(0));
        let tx_state = Arc::new(JoinTxState::<T, U>::new());

        // Subscribe to self (left source)
        let weak1 = derived.downgrade();
        let other1 = other.clone();
        let first1 = Arc::new(AtomicBool::new(true));
        let cs1 = complete_state.clone();
        let tx_state1 = tx_state.clone();
        let other1_for_finalize = other.clone();
        let guard1 = self.subscribe(move |signal| {
            if let Some(d) = weak1.upgrade() {
                match signal {
                    Signal::Value(a, ctx) => {
                        if first1.swap(false, Ordering::SeqCst) {
                            return;
                        }

                        // Check for transaction context
                        if let Some(ctx) = ctx {
                            // Transaction mode: wait for barrier
                            trace!(
                                cell = %derived_id,
                                txid = %ctx.txid,
                                source = "left",
                                "join received value with transaction"
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
                            if let Some((left, right, ctx)) = tx_state1.receive_left(ctx.txid, a.clone(), ctx) {
                                // Barrier met! Push the context forward
                                if let Some(new_ctx) = ctx.push(derived_id) {
                                    trace!(
                                        cell = %derived_id,
                                        txid = %new_ctx.txid,
                                        "join barrier met, firing"
                                    );
                                    d.notify(Signal::Value(
                                        Arc::new(((*left).clone(), (*right).clone())),
                                        Some(new_ctx),
                                    ));
                                }
                            } else {
                                // Barrier not met - register a pending barrier for finalization
                                // This fires if the other source doesn't participate in this transaction
                                let tx_state_fin = tx_state1.clone();
                                let d_fin = d.clone();
                                let ctx_fin = ctx.clone();
                                let other_fin = other1_for_finalize.clone();
                                let derived_id_fin = derived_id;

                                register_pending_barrier(txid, PendingBarrier {
                                    fire_fn: Box::new(move || {
                                        // Check if we already fired (barrier met naturally)
                                        if tx_state_fin.mark_fired(txid) {
                                            let (left_val, _) = tx_state_fin.get_values(txid);
                                            if let Some(left) = left_val
                                                && let Some(new_ctx) = ctx_fin.push(derived_id_fin) {
                                                    trace!(
                                                        cell = %derived_id_fin,
                                                        txid = %txid,
                                                        "join finalization firing (left only)"
                                                    );
                                                    d_fin.notify(Signal::Value(
                                                        Arc::new(((*left).clone(), other_fin.get())),
                                                        Some(new_ctx),
                                                    ));
                                                }
                                            tx_state_fin.cleanup(txid);
                                        }
                                    }),
                                });
                            }
                        } else {
                            // Non-transaction mode: immediate fire
                            d.notify(Signal::value((a.as_ref().clone(), other1.get())));
                        }
                    }
                    Signal::Complete => {
                        let prev = cs1.fetch_or(SELF_COMPLETE, Ordering::SeqCst);
                        if prev == OTHER_COMPLETE {
                            d.notify(Signal::Complete);
                        }
                    }
                    Signal::Error(e) => d.notify(Signal::Error(e.clone())),
                }
            }
        });
        derived.own(guard1);

        // Subscribe to other (right source)
        let weak2 = derived.downgrade();
        let self2 = self.clone();
        let first2 = Arc::new(AtomicBool::new(true));
        let tx_state2 = tx_state;
        let derived_id2 = derived_id;
        let self2_for_finalize = self.clone();
        let guard2 = other.subscribe(move |signal| {
            if let Some(d) = weak2.upgrade() {
                match signal {
                    Signal::Value(b, ctx) => {
                        if first2.swap(false, Ordering::SeqCst) {
                            return;
                        }

                        // Check for transaction context
                        if let Some(ctx) = ctx {
                            // Transaction mode: wait for barrier
                            trace!(
                                cell = %derived_id2,
                                txid = %ctx.txid,
                                source = "right",
                                "join received value with transaction"
                            );

                            // Check for cycle
                            if ctx.would_cycle(derived_id2) {
                                trace!(
                                    cell = %derived_id2,
                                    txid = %ctx.txid,
                                    "cycle detected, skipping"
                                );
                                return;
                            }

                            let txid = ctx.txid;
                            // Try to complete the barrier
                            if let Some((left, right, ctx)) = tx_state2.receive_right(ctx.txid, b.clone(), ctx) {
                                // Barrier met! Push the context forward
                                if let Some(new_ctx) = ctx.push(derived_id2) {
                                    trace!(
                                        cell = %derived_id2,
                                        txid = %new_ctx.txid,
                                        "join barrier met, firing"
                                    );
                                    d.notify(Signal::Value(
                                        Arc::new(((*left).clone(), (*right).clone())),
                                        Some(new_ctx),
                                    ));
                                }
                            } else {
                                // Barrier not met - register a pending barrier for finalization
                                // This fires if the other source doesn't participate in this transaction
                                let tx_state_fin = tx_state2.clone();
                                let d_fin = d.clone();
                                let ctx_fin = ctx.clone();
                                let self_fin = self2_for_finalize.clone();
                                let derived_id_fin = derived_id2;

                                register_pending_barrier(txid, PendingBarrier {
                                    fire_fn: Box::new(move || {
                                        // Check if we already fired (barrier met naturally)
                                        if tx_state_fin.mark_fired(txid) {
                                            let (_, right_val) = tx_state_fin.get_values(txid);
                                            if let Some(right) = right_val
                                                && let Some(new_ctx) = ctx_fin.push(derived_id_fin) {
                                                    trace!(
                                                        cell = %derived_id_fin,
                                                        txid = %txid,
                                                        "join finalization firing (right only)"
                                                    );
                                                    d_fin.notify(Signal::Value(
                                                        Arc::new((self_fin.get(), (*right).clone())),
                                                        Some(new_ctx),
                                                    ));
                                                }
                                            tx_state_fin.cleanup(txid);
                                        }
                                    }),
                                });
                            }
                        } else {
                            // Non-transaction mode: immediate fire
                            d.notify(Signal::value((self2.get(), b.as_ref().clone())));
                        }
                    }
                    Signal::Complete => {
                        let prev = complete_state.fetch_or(OTHER_COMPLETE, Ordering::SeqCst);
                        if prev == SELF_COMPLETE {
                            d.notify(Signal::Complete);
                        }
                    }
                    Signal::Error(e) => d.notify(Signal::Error(e.clone())),
                }
            }
        });
        derived.own(guard2);

        derived.lock()
    }
}

impl<T, W: Watchable<T>> JoinExt<T> for W {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MapExt, Mutable, flat};

    #[test]
    fn test_join_combines_cells() {
        let a = Cell::new(1);
        let b = Cell::new("hello");

        let joined = a.join(&b);
        assert_eq!(joined.get(), (1, "hello"));

        a.set(2);
        assert_eq!(joined.get(), (2, "hello"));

        b.set("world");
        assert_eq!(joined.get(), (2, "world"));
    }

    #[test]
    fn test_flat_macro_chain() {
        let a = Cell::new(1);
        let b = Cell::new(2);
        let c = Cell::new(3);
        let d = Cell::new(4);

        // flat!(|a, b, c, d| ...) expands to |(((a, b), c), d)| ...
        let sum = a.join(&b).join(&c).join(&d).map(flat!(|a, b, c, d| a + b + c + d));

        assert_eq!(sum.get(), 10);
    }
}
