//! How a node learns that no input will deliver a ticket at or below `t`
//! again: the pull-walk of `docs/settle-epochs.md` section 4.
//!
//! The scheduler asks only [`in_w_past`]. On failure it parks on the blocker
//! returned and re-checks (`evaluate` in `settle.rs`), and a blocker wakes
//! its parked nodes when it fires. Section 4 is being redesigned; a new
//! mechanism replaces this module and keeps that contract.

use std::sync::{Arc, Weak, atomic::Ordering};

use rustc_hash::FxHashSet;

use super::{INVALIDATED, NodeCore, QUIET_HITS, SEEDED, VALIDATED, WALK_NODES, WALKS};

pub(super) const NOT_QUIET: u8 = 0;
const TENTATIVE: u8 = 1;
const QUIET: u8 = 2;

fn inputs(node: &NodeCore) -> Vec<Arc<NodeCore>> {
    node.inputs
        .lock()
        .iter()
        .filter_map(Weak::upgrade)
        .collect()
}

fn quiet(node: &NodeCore) -> bool {
    node.mark.load(Ordering::Acquire) == QUIET
}

/// True when every input of `core` has promised past `ticket`: `in_w(n) >
/// ticket`. Otherwise the node found holding work at or below `ticket`, for
/// the caller to park on.
///
/// A quiet input counts as `S`, which is past every seeded ticket. An input
/// that is not quiet is certified if it can be; otherwise its watermark is
/// pulled from its ancestry.
pub(super) fn in_w_past(core: &Arc<NodeCore>, ticket: u64) -> Result<(), Arc<NodeCore>> {
    for input in inputs(core) {
        if quiet(&input) {
            QUIET_HITS.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        if input.watermark.load(Ordering::Acquire) > ticket {
            continue;
        }
        if validate(&input, &mut FxHashSet::default()) {
            continue;
        }
        WALKS.fetch_add(1, Ordering::Relaxed);
        let mut seen = FxHashSet::default();
        seen.insert(Arc::as_ptr(core).addr());
        let mut cost = 0;
        let outcome = passed(&input, ticket, &mut seen, &mut cost);
        WALK_NODES.fetch_add(cost, Ordering::Relaxed);
        outcome?;
    }
    Ok(())
}

/// The bound `in_w` gives a node's watermark when it fires: `S` for a quiet
/// input, its watermark otherwise. O(inputs).
pub(super) fn refresh_bound(core: &NodeCore) -> u64 {
    let mut low = SEEDED.load(Ordering::Acquire);
    for input in inputs(core) {
        if !quiet(&input) {
            low = low.min(input.watermark.load(Ordering::Acquire));
        }
    }
    low
}

/// Clear the certificates of `node` and of every certified descendant. A
/// node that is not certified has no certified descendant, so the walk stops
/// there.
pub(super) fn invalidate(node: &Arc<NodeCore>) {
    let mut stack = vec![Arc::clone(node)];
    while let Some(node) = stack.pop() {
        if node.mark.swap(NOT_QUIET, Ordering::AcqRel) != NOT_QUIET {
            INVALIDATED.fetch_add(1, Ordering::Relaxed);
            stack.extend(node.outputs.lock().iter().filter_map(Weak::upgrade));
        }
    }
}

/// Clear a certificate that a push from a fire found. A push along a
/// declared edge from the firing node never finds one, because the firing
/// node is in the target's ancestry and holds the ticket.
pub(super) fn invalidate_if_marked(node: &Arc<NodeCore>) {
    if node.mark.load(Ordering::Acquire) != NOT_QUIET {
        invalidate(node);
    }
}

/// Certify `node` quiet, ancestors first: nothing in its ancestry holds
/// pending work. Check, mark tentative, check again, then promote; a seed in
/// between clears the tentative mark and the promotion fails.
fn validate(node: &Arc<NodeCore>, seen: &mut FxHashSet<usize>) -> bool {
    if quiet(node) {
        return true;
    }
    if !seen.insert(Arc::as_ptr(node).addr()) {
        return false;
    }
    let ancestors = inputs(node);
    if !ancestors.iter().all(|input| validate(input, seen)) {
        return false;
    }
    let holds = |node: &NodeCore| node.idle() && ancestors.iter().all(|input| quiet(input));
    if !holds(node) {
        return false;
    }
    node.mark.store(TENTATIVE, Ordering::Release);
    if !holds(node) {
        return false;
    }
    let promoted = node
        .mark
        .compare_exchange(TENTATIVE, QUIET, Ordering::AcqRel, Ordering::Acquire)
        .is_ok();
    if promoted {
        VALIDATED.fetch_add(1, Ordering::Relaxed);
    }
    promoted
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
    if quiet(m) || m.watermark.load(Ordering::Acquire) > ticket {
        return Ok(());
    }
    if !seen.insert(Arc::as_ptr(m).addr()) {
        // Visited in this walk and not past `ticket`: it is on the current
        // path, which is a cycle.
        return Err(Arc::clone(m));
    }
    *cost = cost.saturating_add(1);
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
    let previous = m.watermark.fetch_max(low.min(min), Ordering::AcqRel);
    debug_assert!(
        min >= previous,
        "hyphae: promise safety: watermark {previous} was above pending {min}"
    );
    drop(pending);
    Ok(())
}
