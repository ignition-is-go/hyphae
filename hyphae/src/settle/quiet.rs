//! Quiet certificates, section 4 of `docs/settle-epochs.md`, kept beside the
//! pull walk in [`super::shadow`] so the two can be compared on one workload.
//!
//! A node marked `QUIET` promises that nothing in its ancestry, itself
//! included, holds pending work. A seed clears the marks of its descendants,
//! stopping at `NONE`. A join that finds a silent input not `QUIET` validates
//! the input's ancestry bottom-up: check, write `TENTATIVE`, re-check, then
//! `compare_exchange(TENTATIVE, QUIET)`.

use std::sync::{
    Arc,
    atomic::{AtomicU8, AtomicU64, Ordering},
};

use rustc_hash::FxHashSet;
use uuid::Uuid;

use super::{
    Node,
    shadow::{AtomicHistogram, Histogram, Shadow, direct_inputs},
};
use crate::traits::DepNode;

const NONE: u8 = 0;
const TENTATIVE: u8 = 1;
const QUIET: u8 = 2;
/// Set beside the mark once a node has been validated, so a later clear of it
/// can be told apart from a node that was never quiet.
const VALIDATED_ONCE: u8 = 4;
const MARK: u8 = 3;

/// One node's certificate.
#[derive(Default)]
pub(super) struct Mark(AtomicU8);

impl Mark {
    fn get(&self) -> u8 {
        self.0.load(Ordering::Acquire) & MARK
    }

    fn validated_once(&self) -> bool {
        self.0.load(Ordering::Acquire) & VALIDATED_ONCE != 0
    }

    /// Clear to `NONE`. False when it already was.
    fn clear(&self) -> bool {
        self.0.fetch_and(!MARK, Ordering::AcqRel) & MARK != NONE
    }

    fn write_tentative(&self) {
        self.0.fetch_and(!MARK, Ordering::AcqRel);
        self.0.fetch_or(TENTATIVE, Ordering::AcqRel);
    }

    fn promote(&self) -> bool {
        let current = self.0.load(Ordering::Acquire);
        if current & MARK != TENTATIVE {
            return false;
        }
        self.0
            .compare_exchange(
                current,
                (current & !MARK) | QUIET | VALIDATED_ONCE,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
    }
}

/// Quiet-certificate counters. See [`super::ShadowStats::quiet`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QuietStats {
    /// Seeds: writes placed while an event set is seeded.
    pub seeds: u64,
    /// Nodes visited by clearing walks, summed; the seed node included.
    pub clear_nodes_total: u64,
    /// Nanoseconds spent clearing, summed.
    pub clear_nanos_total: u64,
    /// Nodes visited per clearing walk.
    pub clear_nodes: Histogram,
    /// Nanoseconds per clearing walk.
    pub clear_nanos: Histogram,
    /// Marks a clear moved from `QUIET` or `TENTATIVE` to `NONE`.
    pub cleared_marks: u64,
    /// Pushes along an undeclared edge into a `QUIET` node, which break its
    /// certificate without a seed. The shadow clears from there.
    pub broken_by_push: u64,
    /// Input reads at fires.
    pub input_reads: u64,
    /// Reads of an input that delivered in this fire.
    pub delivered_reads: u64,
    /// Reads answered by a `QUIET` mark.
    pub quiet_reads: u64,
    /// Validations: reads of a silent input that was not `QUIET`.
    pub validations: u64,
    /// Validations of an input that had been `QUIET` before: a seed landed in
    /// its ancestry since it was last validated.
    pub seed_hits: u64,
    /// Validations that ended without `QUIET`: pending work, or a seed that
    /// cleared a mark before the exchange.
    pub validation_fails: u64,
    /// Nodes visited by validations, summed.
    pub validation_nodes_total: u64,
    /// Nanoseconds spent validating, summed.
    pub validation_nanos_total: u64,
    /// Nodes visited per validation.
    pub validation_nodes: Histogram,
    /// Nanoseconds per validation.
    pub validation_nanos: Histogram,
}

static SEEDS: AtomicU64 = AtomicU64::new(0);
static CLEAR_NODES_TOTAL: AtomicU64 = AtomicU64::new(0);
static CLEAR_NANOS_TOTAL: AtomicU64 = AtomicU64::new(0);
static CLEAR_NODES: AtomicHistogram = AtomicHistogram::new();
static CLEAR_NANOS: AtomicHistogram = AtomicHistogram::new();
static CLEARED_MARKS: AtomicU64 = AtomicU64::new(0);
static BROKEN_BY_PUSH: AtomicU64 = AtomicU64::new(0);
static INPUT_READS: AtomicU64 = AtomicU64::new(0);
static DELIVERED_READS: AtomicU64 = AtomicU64::new(0);
static QUIET_READS: AtomicU64 = AtomicU64::new(0);
static VALIDATIONS: AtomicU64 = AtomicU64::new(0);
static SEED_HITS: AtomicU64 = AtomicU64::new(0);
static VALIDATION_FAILS: AtomicU64 = AtomicU64::new(0);
static VALIDATION_NODES_TOTAL: AtomicU64 = AtomicU64::new(0);
static VALIDATION_NANOS_TOTAL: AtomicU64 = AtomicU64::new(0);
static VALIDATION_NODES: AtomicHistogram = AtomicHistogram::new();
static VALIDATION_NANOS: AtomicHistogram = AtomicHistogram::new();

pub(super) fn stats() -> QuietStats {
    let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
    QuietStats {
        seeds: load(&SEEDS),
        clear_nodes_total: load(&CLEAR_NODES_TOTAL),
        clear_nanos_total: load(&CLEAR_NANOS_TOTAL),
        clear_nodes: CLEAR_NODES.load(),
        clear_nanos: CLEAR_NANOS.load(),
        cleared_marks: load(&CLEARED_MARKS),
        broken_by_push: load(&BROKEN_BY_PUSH),
        input_reads: load(&INPUT_READS),
        delivered_reads: load(&DELIVERED_READS),
        quiet_reads: load(&QUIET_READS),
        validations: load(&VALIDATIONS),
        seed_hits: load(&SEED_HITS),
        validation_fails: load(&VALIDATION_FAILS),
        validation_nodes_total: load(&VALIDATION_NODES_TOTAL),
        validation_nanos_total: load(&VALIDATION_NANOS_TOTAL),
        validation_nodes: VALIDATION_NODES.load(),
        validation_nanos: VALIDATION_NANOS.load(),
    }
}

fn nanos_since(started: crate::platform::Instant) -> u64 {
    u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

/// A write reached `node`. A seed clears the node and its marked
/// descendants; so does a push into a `QUIET` node, which only an undeclared
/// edge can make.
pub(super) fn pushed(node: &Arc<dyn Node>, seeding: bool) {
    let quiet = node.shadow().mark.get() == QUIET;
    if !seeding && !quiet {
        return;
    }
    if seeding {
        SEEDS.fetch_add(1, Ordering::Relaxed);
    } else {
        BROKEN_BY_PUSH.fetch_add(1, Ordering::Relaxed);
    }
    let started = crate::platform::Instant::now();
    let mut visited = 1u64;
    let mut cleared = u64::from(node.shadow().mark.clear());
    let mut stack = node.node_dependents();
    while let Some(weak) = stack.pop() {
        let Some(dependent) = weak.upgrade() else {
            continue;
        };
        visited = visited.saturating_add(1);
        if dependent.shadow().mark.clear() {
            cleared = cleared.saturating_add(1);
            stack.extend(dependent.node_dependents());
        }
    }
    let nanos = nanos_since(started);
    CLEARED_MARKS.fetch_add(cleared, Ordering::Relaxed);
    CLEAR_NODES_TOTAL.fetch_add(visited, Ordering::Relaxed);
    CLEAR_NANOS_TOTAL.fetch_add(nanos, Ordering::Relaxed);
    CLEAR_NODES.record(visited);
    CLEAR_NANOS.record(nanos);
}

/// Read each input of a node about to fire the tickets from `lo` up, as the
/// quiet-certificate design would.
pub(super) fn read_inputs(inputs: &[Arc<dyn DepNode>], lo: u64) {
    for input in inputs {
        let Some(shadow) = input.shadow() else {
            continue;
        };
        INPUT_READS.fetch_add(1, Ordering::Relaxed);
        if shadow.processed() >= lo {
            DELIVERED_READS.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        if shadow.mark.get() == QUIET {
            QUIET_READS.fetch_add(1, Ordering::Relaxed);
            continue;
        }
        VALIDATIONS.fetch_add(1, Ordering::Relaxed);
        if shadow.mark.validated_once() {
            SEED_HITS.fetch_add(1, Ordering::Relaxed);
        }
        let started = crate::platform::Instant::now();
        let mut validation = Validation::default();
        if !validation.validate(input.as_ref()) {
            VALIDATION_FAILS.fetch_add(1, Ordering::Relaxed);
        }
        let nanos = nanos_since(started);
        VALIDATION_NODES_TOTAL.fetch_add(validation.visited, Ordering::Relaxed);
        VALIDATION_NANOS_TOTAL.fetch_add(nanos, Ordering::Relaxed);
        VALIDATION_NODES.record(validation.visited);
        VALIDATION_NANOS.record(nanos);
    }
}

#[derive(Default)]
struct Validation {
    visited: u64,
    seen: FxHashSet<Uuid>,
}

impl Validation {
    /// Mark `node` and its not-yet-`QUIET` ancestry `QUIET`, inputs first.
    fn validate(&mut self, node: &dyn DepNode) -> bool {
        self.visited = self.visited.saturating_add(1);
        let Some(shadow) = node.shadow() else {
            return direct_inputs(node.deps())
                .iter()
                .all(|input| self.validate(input.as_ref()));
        };
        if shadow.mark.get() == QUIET {
            return true;
        }
        if !self.seen.insert(node.id()) {
            return false;
        }
        let inputs = direct_inputs(node.deps());
        if !Self::clear_now(shadow, &inputs) {
            return false;
        }
        if !inputs.iter().all(|input| self.validate(input.as_ref())) {
            return false;
        }
        shadow.mark.write_tentative();
        Self::clear_now(shadow, &inputs) && shadow.mark.promote()
    }

    /// The check, and the re-check after `TENTATIVE`: nothing pending here.
    /// Inputs must be `QUIET` as well; before the recursion that holds only
    /// for the inputs already marked, so it is checked after it too.
    fn clear_now(shadow: &Shadow, inputs: &[Arc<dyn DepNode>]) -> bool {
        shadow.is_idle()
            && inputs.iter().all(|input| {
                input
                    .shadow()
                    .is_none_or(|input| input.mark.get() != NONE || input.is_idle())
            })
    }
}

#[cfg(test)]
mod tests {
    use crate::{Cell, JoinExt, Materialize, Mutable, settle::stats};

    #[test]
    fn a_quiet_input_is_read_without_a_walk_until_a_seed_reaches_it() {
        let a = Cell::new(1);
        let b = Cell::new(2);
        let _joined = a.clone().join(b.clone()).materialize();
        a.set(3);
        let validated = stats().shadow.quiet;
        a.set(4);
        let read = stats().shadow.quiet;
        assert!(read.quiet_reads > validated.quiet_reads, "b stayed quiet");
        b.set(5);
        let seeded = stats().shadow.quiet;
        assert!(
            seeded.cleared_marks > read.cleared_marks,
            "the seed cleared b"
        );
        a.set(6);
        let hit = stats().shadow.quiet;
        assert!(
            hit.seed_hits > seeded.seed_hits,
            "b was quiet before the seed"
        );
    }
}
