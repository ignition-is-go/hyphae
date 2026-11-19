//! Shadow tickets: the per-node bookkeeping of `docs/settle-epochs.md`, kept
//! beside today's settle to measure what the watermark pull would cost.
//! Nothing here changes what is delivered or when.
//!
//! Every event set takes a ticket when the holder seeds it. A write that joins
//! the settle carries the tickets of the set or fire that made it, so each
//! node knows the tickets it holds when it fires. Before a fire, the node
//! computes its input watermark as the design does, pulling silent inputs.

use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use rustc_hash::FxHashSet;
use uuid::Uuid;

use super::Node;
use crate::traits::DepNode;

const NO_TICKET: u64 = u64::MAX;

/// One node's ticket state: `p`, `w`, and the range of tickets pending on it.
#[doc(hidden)]
pub struct Shadow {
    processed: AtomicU64,
    watermark: AtomicU64,
    pending_lo: AtomicU64,
    pending_hi: AtomicU64,
}

impl Default for Shadow {
    fn default() -> Self {
        Self {
            processed: AtomicU64::new(0),
            watermark: AtomicU64::new(0),
            pending_lo: AtomicU64::new(NO_TICKET),
            pending_hi: AtomicU64::new(0),
        }
    }
}

/// The tickets a write carries: those of the event set being seeded, or of
/// the fire that made it.
#[derive(Clone, Copy)]
pub(super) struct Tickets {
    lo: u64,
    hi: u64,
}

impl Tickets {
    pub(super) const NONE: Self = Self {
        lo: NO_TICKET,
        hi: 0,
    };

    pub(super) const fn one(ticket: u64) -> Self {
        Self {
            lo: ticket,
            hi: ticket,
        }
    }
}

static NEXT_TICKET: AtomicU64 = AtomicU64::new(1);
/// `S`: every ticket below it has been seeded.
static SEEDED: AtomicU64 = AtomicU64::new(1);

/// Take the next ticket. Only the settle holder calls it, in seeding order.
pub(super) fn admit() -> u64 {
    NEXT_TICKET.fetch_add(1, Ordering::Relaxed)
}

pub(super) fn seeded_through(next: u64) {
    SEEDED.store(next, Ordering::Release);
}

/// Bucketed counts: four buckets per power of two, so a quantile read from
/// it is within 25% of the true value.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Histogram(pub [u64; BUCKETS]);

const BUCKETS: usize = 256;

impl Default for Histogram {
    fn default() -> Self {
        Self([0; BUCKETS])
    }
}

impl std::fmt::Debug for Histogram {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_map()
            .entries(
                self.0
                    .iter()
                    .enumerate()
                    .filter(|(_, count)| **count > 0)
                    .map(|(bucket, count)| (bucket_floor(bucket), count)),
            )
            .finish()
    }
}

impl Histogram {
    /// The lower bound of the bucket holding the `per_mille`/1000 quantile,
    /// or 0 when empty.
    #[must_use]
    pub fn quantile(&self, per_mille: u64) -> u64 {
        let total = self
            .0
            .iter()
            .fold(0u64, |sum, count| sum.saturating_add(*count));
        if total == 0 {
            return 0;
        }
        let rank = total
            .saturating_mul(per_mille)
            .div_ceil(1000)
            .clamp(1, total);
        let mut seen = 0u64;
        for (bucket, count) in self.0.iter().enumerate() {
            seen = seen.saturating_add(*count);
            if seen >= rank {
                return bucket_floor(bucket);
            }
        }
        0
    }

    /// The lower bound of the highest non-empty bucket.
    #[must_use]
    pub fn max(&self) -> u64 {
        self.0
            .iter()
            .rposition(|count| *count > 0)
            .map_or(0, bucket_floor)
    }
}

fn bucket(value: u64) -> usize {
    if value < 4 {
        return usize::try_from(value).unwrap_or(0);
    }
    let octave = 63u32.wrapping_sub(value.leading_zeros());
    let sub = (value >> octave.wrapping_sub(2)) & 3;
    usize::try_from(
        u64::from(octave.wrapping_sub(1))
            .wrapping_mul(4)
            .wrapping_add(sub),
    )
    .unwrap_or(0)
}

fn bucket_floor(bucket: usize) -> u64 {
    let bucket = u64::try_from(bucket).unwrap_or(0);
    if bucket < 4 {
        return bucket;
    }
    let octave = (bucket / 4).wrapping_add(1);
    (4u64.wrapping_add(bucket % 4)) << octave.wrapping_sub(2)
}

struct AtomicHistogram([AtomicU64; BUCKETS]);

impl AtomicHistogram {
    const fn new() -> Self {
        Self([const { AtomicU64::new(0) }; BUCKETS])
    }

    fn record(&self, value: u64) {
        if let Some(slot) = self.0.get(bucket(value)) {
            slot.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn load(&self) -> Histogram {
        Histogram(self.0.each_ref().map(|slot| slot.load(Ordering::Relaxed)))
    }
}

/// Shadow-ticket counters. See [`super::SettleStats::shadow`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ShadowStats {
    /// Tickets taken: one per seeded event set.
    pub tickets: u64,
    /// Fires that held at least one ticket.
    pub fires: u64,
    /// Fires that held no ticket: a write landed between a fire's clear and
    /// its take.
    pub empty_fires: u64,
    /// Fires of a node with two or more inputs.
    pub join_fires: u64,
    /// Join fires where at least one input was silent and had to be pulled.
    pub join_silent_fires: u64,
    /// Fires, join or not, that needed at least one pull.
    pub walked_fires: u64,
    /// Pulls: one per silent input of a fire.
    pub walks: u64,
    /// Pulls that stopped only at nodes whose cached watermark was past the
    /// ticket.
    pub walks_ended_past: u64,
    /// Pulls that reached a node with no inputs, whose watermark is `S`.
    pub walks_ended_s: u64,
    /// Pulls that met a node still holding a ticket at or below the fire's,
    /// where the design would park.
    pub walks_parked: u64,
    /// Fires the design would not have allowed yet: the input watermark was
    /// not past the ticket after the pulls.
    pub blocked_fires: u64,
    /// Pushes at or below the target's `p`, or below its watermark: a write
    /// along an undeclared edge that the design would make a new ticket.
    pub late_pushes: u64,
    /// Fires whose lowest ticket was at or below the node's `p`.
    pub order_violations: u64,
    /// Nodes visited, summed over pulls.
    pub walk_nodes_total: u64,
    /// Nanoseconds spent pulling, summed.
    pub walk_nanos_total: u64,
    /// Nodes visited per pull.
    pub walk_nodes: Histogram,
    /// Nanoseconds per pull.
    pub walk_nanos: Histogram,
    /// Pulls per fire, over fires that pulled.
    pub walks_per_walked_fire: Histogram,
}

static FIRES: AtomicU64 = AtomicU64::new(0);
static EMPTY_FIRES: AtomicU64 = AtomicU64::new(0);
static JOIN_FIRES: AtomicU64 = AtomicU64::new(0);
static JOIN_SILENT_FIRES: AtomicU64 = AtomicU64::new(0);
static WALKED_FIRES: AtomicU64 = AtomicU64::new(0);
static WALKS: AtomicU64 = AtomicU64::new(0);
static WALKS_ENDED_PAST: AtomicU64 = AtomicU64::new(0);
static WALKS_ENDED_S: AtomicU64 = AtomicU64::new(0);
static WALKS_PARKED: AtomicU64 = AtomicU64::new(0);
static BLOCKED_FIRES: AtomicU64 = AtomicU64::new(0);
static LATE_PUSHES: AtomicU64 = AtomicU64::new(0);
static ORDER_VIOLATIONS: AtomicU64 = AtomicU64::new(0);
static WALK_NODES_TOTAL: AtomicU64 = AtomicU64::new(0);
static WALK_NANOS_TOTAL: AtomicU64 = AtomicU64::new(0);
static WALK_NODES: AtomicHistogram = AtomicHistogram::new();
static WALK_NANOS: AtomicHistogram = AtomicHistogram::new();
static WALKS_PER_WALKED_FIRE: AtomicHistogram = AtomicHistogram::new();

pub(super) fn stats() -> ShadowStats {
    let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
    ShadowStats {
        tickets: load(&NEXT_TICKET).saturating_sub(1),
        fires: load(&FIRES),
        empty_fires: load(&EMPTY_FIRES),
        join_fires: load(&JOIN_FIRES),
        join_silent_fires: load(&JOIN_SILENT_FIRES),
        walked_fires: load(&WALKED_FIRES),
        walks: load(&WALKS),
        walks_ended_past: load(&WALKS_ENDED_PAST),
        walks_ended_s: load(&WALKS_ENDED_S),
        walks_parked: load(&WALKS_PARKED),
        blocked_fires: load(&BLOCKED_FIRES),
        late_pushes: load(&LATE_PUSHES),
        order_violations: load(&ORDER_VIOLATIONS),
        walk_nodes_total: load(&WALK_NODES_TOTAL),
        walk_nanos_total: load(&WALK_NANOS_TOTAL),
        walk_nodes: WALK_NODES.load(),
        walk_nanos: WALK_NANOS.load(),
        walks_per_walked_fire: WALKS_PER_WALKED_FIRE.load(),
    }
}

/// Record that a write carrying `tickets` reached `target`.
pub(super) fn push(target: &Shadow, tickets: Tickets) {
    if tickets.lo == NO_TICKET {
        return;
    }
    if tickets.lo <= target.processed.load(Ordering::Acquire)
        || tickets.lo < target.watermark.load(Ordering::Acquire)
    {
        LATE_PUSHES.fetch_add(1, Ordering::Relaxed);
    }
    target.pending_lo.fetch_min(tickets.lo, Ordering::AcqRel);
    target.pending_hi.fetch_max(tickets.hi, Ordering::AcqRel);
}

/// Account for `node` firing now. Returns the tickets its writes carry.
pub(super) fn fire(node: &dyn Node) -> Tickets {
    let shadow = node.shadow();
    let lo = shadow.pending_lo.swap(NO_TICKET, Ordering::AcqRel);
    let hi = shadow.pending_hi.swap(0, Ordering::AcqRel);
    if lo == NO_TICKET {
        EMPTY_FIRES.fetch_add(1, Ordering::Relaxed);
        return Tickets::NONE;
    }
    FIRES.fetch_add(1, Ordering::Relaxed);
    let processed = shadow.processed.load(Ordering::Acquire);
    if lo <= processed {
        ORDER_VIOLATIONS.fetch_add(1, Ordering::Relaxed);
    }
    #[cfg(any(debug_assertions, test))]
    assert!(
        lo > processed,
        "per-node ticket order: node {} fires ticket {lo} after ticket {processed}",
        node.node_id()
    );
    let ticket = hi;
    let seeded = SEEDED.load(Ordering::Acquire);
    let mut in_w = seeded;
    let mut inputs = 0u64;
    let mut walks = 0u64;
    for input in direct_inputs(node.shadow_inputs()) {
        let mut walk = Walk::new(ticket, seeded);
        let started = crate::platform::Instant::now();
        let contrib = walk.refresh(input.as_ref());
        if contrib == NO_TICKET {
            continue;
        }
        inputs = inputs.saturating_add(1);
        in_w = in_w.min(contrib);
        if walk.refreshed > 0 {
            walks = walks.saturating_add(1);
            walk.record(started);
        }
    }
    if inputs >= 2 {
        JOIN_FIRES.fetch_add(1, Ordering::Relaxed);
        if walks > 0 {
            JOIN_SILENT_FIRES.fetch_add(1, Ordering::Relaxed);
        }
    }
    if walks > 0 {
        WALKED_FIRES.fetch_add(1, Ordering::Relaxed);
        WALKS_PER_WALKED_FIRE.record(walks);
    }
    if in_w <= ticket {
        BLOCKED_FIRES.fetch_add(1, Ordering::Relaxed);
    }
    shadow.processed.fetch_max(hi, Ordering::AcqRel);
    shadow.watermark.fetch_max(in_w, Ordering::AcqRel);
    Tickets { lo, hi }
}

/// The inputs that carry tickets, looking through transparent nodes such as
/// a pipeline's composite guard, which stands for several real inputs.
fn direct_inputs(inputs: Vec<Arc<dyn DepNode>>) -> Vec<Arc<dyn DepNode>> {
    let mut direct = Vec::with_capacity(inputs.len());
    let mut stack = inputs;
    while let Some(input) = stack.pop() {
        if input.shadow().is_some() {
            direct.push(input);
        } else {
            stack.extend(input.deps());
        }
    }
    direct
}

/// One pull of a silent input: section 4's refresh, memoized in the cached
/// watermarks it moves forward.
struct Walk {
    ticket: u64,
    seeded: u64,
    visited: u64,
    refreshed: u64,
    reached_s: bool,
    parked: bool,
    seen: FxHashSet<Uuid>,
}

impl Walk {
    fn new(ticket: u64, seeded: u64) -> Self {
        Self {
            ticket,
            seeded,
            visited: 0,
            refreshed: 0,
            reached_s: false,
            parked: false,
            seen: FxHashSet::default(),
        }
    }

    /// The watermark `node` contributes, refreshing it if it is not past the
    /// ticket. [`NO_TICKET`] for a node that carries no tickets at all, such as
    /// a callback guard.
    fn refresh(&mut self, node: &dyn DepNode) -> u64 {
        self.visited = self.visited.saturating_add(1);
        let Some(shadow) = node.shadow() else {
            return self.min_over(node.deps());
        };
        let cached = shadow.watermark.load(Ordering::Acquire);
        if cached > self.ticket || !self.seen.insert(node.id()) {
            return cached;
        }
        self.refreshed = self.refreshed.saturating_add(1);
        let pending = shadow.pending_lo.load(Ordering::Acquire);
        if pending <= self.ticket {
            self.parked = true;
            return pending;
        }
        let from_inputs = self.min_over(node.deps());
        if from_inputs == NO_TICKET {
            self.reached_s = true;
        }
        let refreshed = from_inputs.min(self.seeded).min(pending);
        shadow.watermark.fetch_max(refreshed, Ordering::AcqRel);
        refreshed
    }

    fn min_over(&mut self, inputs: Vec<Arc<dyn DepNode>>) -> u64 {
        let mut low = NO_TICKET;
        for input in inputs {
            low = low.min(self.refresh(input.as_ref()));
            if low <= self.ticket {
                break;
            }
        }
        low
    }

    fn record(&self, started: crate::platform::Instant) {
        let nanos = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
        WALKS.fetch_add(1, Ordering::Relaxed);
        let end = if self.parked {
            &WALKS_PARKED
        } else if self.reached_s {
            &WALKS_ENDED_S
        } else {
            &WALKS_ENDED_PAST
        };
        end.fetch_add(1, Ordering::Relaxed);
        WALK_NODES_TOTAL.fetch_add(self.visited, Ordering::Relaxed);
        WALK_NANOS_TOTAL.fetch_add(nanos, Ordering::Relaxed);
        WALK_NODES.record(self.visited);
        WALK_NANOS.record(nanos);
    }
}

#[cfg(test)]
mod tests {
    use super::{bucket, bucket_floor};
    use crate::{Cell, JoinExt, Materialize, Mutable, settle::stats};

    #[test]
    fn a_join_pulls_its_silent_input() {
        let a = Cell::new(1);
        let b = Cell::new(2);
        let _joined = a.clone().join(b).materialize();
        let before = stats().shadow;
        a.set(3);
        let after = stats().shadow;
        assert!(
            after.join_silent_fires > before.join_silent_fires,
            "the join heard only a"
        );
        assert!(
            after.walks_ended_s > before.walks_ended_s,
            "b has no inputs, so its pull reads S"
        );
        assert_eq!(after.order_violations, 0);
    }

    #[test]
    fn buckets_round_down_within_a_quarter_octave() {
        for value in [0, 1, 3, 4, 5, 7, 8, 9, 15, 16, 100, 1_000_000, u64::MAX] {
            let floor = bucket_floor(bucket(value));
            assert!(
                floor <= value && value - floor <= value / 4,
                "{value} -> {floor}"
            );
        }
    }
}
