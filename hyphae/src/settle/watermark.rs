//! How a node learns that no input will deliver a ticket at or below `t`
//! again: the pull-walk of `docs/settle-epochs.md` section 4.
//!
//! The scheduler asks only [`in_w_past`]. On failure it parks on the blocker
//! returned and re-checks (`evaluate` in `settle.rs`), and a blocker wakes
//! its parked nodes when it fires. Section 4 is being redesigned; a new
//! mechanism replaces this module and keeps that contract.

use std::sync::{Arc, Weak, atomic::Ordering};

use rustc_hash::FxHashSet;

use super::{NodeCore, SEEDED, WALK_NODES, WALKS};

/// True when every input of `core` has promised past `ticket`: `in_w(n) >
/// ticket`. Otherwise the node found holding work at or below `ticket`, for
/// the caller to park on.
pub(super) fn in_w_past(core: &Arc<NodeCore>, ticket: u64) -> Result<(), Arc<NodeCore>> {
    let inputs: Vec<Arc<NodeCore>> = core
        .inputs
        .lock()
        .iter()
        .filter_map(Weak::upgrade)
        .collect();
    if inputs.is_empty() {
        return Ok(());
    }
    WALKS.fetch_add(1, Ordering::Relaxed);
    let mut seen = FxHashSet::default();
    seen.insert(Arc::as_ptr(core).addr());
    let mut cost = 0;
    let outcome = inputs
        .iter()
        .try_for_each(|input| passed(input, ticket, &mut seen, &mut cost));
    WALK_NODES.fetch_add(cost, Ordering::Relaxed);
    outcome
}

/// Whether `m` has promised past `ticket`, refreshing its watermark from its
/// inputs. On failure, the node found holding work at or below `ticket`.
///
/// Inputs are read before the node's own pending list: an input's watermark
/// passes a ticket only after its push of that ticket landed here.
fn passed(
    m: &Arc<NodeCore>,
    ticket: u64,
    seen: &mut FxHashSet<usize>,
    cost: &mut u64,
) -> Result<(), Arc<NodeCore>> {
    if m.watermark.load(Ordering::Acquire) > ticket {
        return Ok(());
    }
    if !seen.insert(Arc::as_ptr(m).addr()) {
        // Visited in this walk and not past `ticket`: it is on the current
        // path, which is a cycle.
        return Err(Arc::clone(m));
    }
    *cost += 1;
    let mut low = SEEDED.load(Ordering::Acquire);
    if low <= ticket {
        return Err(Arc::clone(m));
    }
    let inputs: Vec<Arc<NodeCore>> = m.inputs.lock().iter().filter_map(Weak::upgrade).collect();
    for input in &inputs {
        passed(input, ticket, seen, cost)?;
        low = low.min(input.watermark.load(Ordering::Acquire));
    }
    let pending = m.pending.lock();
    let min = pending.min();
    if min <= ticket {
        return Err(Arc::clone(m));
    }
    m.watermark.fetch_max(low.min(min), Ordering::AcqRel);
    drop(pending);
    Ok(())
}

