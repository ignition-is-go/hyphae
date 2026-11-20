//! Readiness from reachability (`docs/settle-epochs-model/reach_model.rs`).
//!
//! Node `n` fires its lowest pending ticket `t` when no pending entry
//! `u <= t` at another node can reach `n`. Two structures answer it:
//!
//! - **The label index.** Every node has two slots in one ordered list, a
//!   start and an end, kept in an order-maintenance structure (list
//!   labelling: an insert takes the midpoint key, and a full key range is
//!   relabelled locally, over the smallest aligned window sparse enough).
//!   Starts follow a topological order of the declared edges. A new node's
//!   pair goes right after its first input's start, inside that input's
//!   pair, so the input and every ancestor that covers it already cover the
//!   new node. A label is a list of slot pairs `[start, end]` covering every
//!   node a node reaches; relabelling keeps the order, so labels never need
//!   rewriting. Every other edge adds the target's label to the source and
//!   its ancestors, eagerly. An edge into a node whose start is not after
//!   the input's (a family row built after its reader) gives the node a new
//!   pair right after its latest input's start, when that is still before
//!   every output. Only a true order conflict, a label overflow, or too many
//!   dead slots lays the list out again, as a compaction.
//!   Labels are never shrunk when a node or edge goes away: a stale pair
//!   only adds waiting.
//! - **The frontier.** For each ticket in flight, the nodes that hold it
//!   pending. A readiness check scans the entries at or below its ticket and
//!   rejects most by order: `f` cannot reach `n` when `key(f) >= key(n)`.
//!
//! A gated edge `src -> n` (a family into its reader) is not in the labels.
//! It counts at ticket `t` only while its gate's delivery of `t` is pending
//! at `n`, which is when `n` is the node asking: the caller passes the open
//! gates, and an entry at `src`, or one that reaches `src`, blocks.
//!
//! Locks, in order: the frontier, then `ORDER`, then a node's label. A node
//! drops (and takes the frontier lock) only where no `ORDER` write guard and
//! no frontier guard is held.

use std::{
    collections::BTreeMap,
    sync::{
        Arc, LazyLock, Weak,
        atomic::{AtomicU64, Ordering},
    },
    time::Instant,
};

use parking_lot::{Mutex, RwLock, RwLockUpgradableReadGuard};
use rustc_hash::{FxHashMap, FxHashSet};

use super::NodeCore;

/// The slot of a node no declared edge has touched. Nothing reaches it.
pub(super) const UNASSIGNED: u64 = u64::MAX;

pub(super) static LABEL_QUERIES: AtomicU64 = AtomicU64::new(0);
pub(super) static FRONTIER_SCANNED: AtomicU64 = AtomicU64::new(0);
pub(super) static MAX_FRONTIER: AtomicU64 = AtomicU64::new(0);
/// Compactions, by cause, and their total time.
pub(super) static REBUILD_OVERFLOW: AtomicU64 = AtomicU64::new(0);
pub(super) static REBUILD_DEAD: AtomicU64 = AtomicU64::new(0);
pub(super) static REBUILD_NANOS: AtomicU64 = AtomicU64::new(0);
/// Local relabels of the order, and the slots they moved.
pub(super) static RELABELS: AtomicU64 = AtomicU64::new(0);
pub(super) static RELABELED_SLOTS: AtomicU64 = AtomicU64::new(0);
/// Edges that landed against the order, and the nodes their local reorders
/// moved.
pub(super) static REORDERS: AtomicU64 = AtomicU64::new(0);
pub(super) static REORDERED_NODES: AtomicU64 = AtomicU64::new(0);
pub(super) static CYCLES: AtomicU64 = AtomicU64::new(0);
/// Back edges a compaction left out of the labels, `(from site, to site) ->
/// count`.
static CYCLE_SITES: Sites = LazyLock::new(Mutex::default);
/// Edges that landed against the order, `(input site, target site) ->
/// count`.
static CONFLICT_SITES: Sites = LazyLock::new(Mutex::default);

type Sites = LazyLock<Mutex<FxHashMap<(String, String), u64>>>;

/// Every compaction, whatever its cause.
pub(super) fn rebuilds() -> u64 {
    REBUILD_OVERFLOW.load(Ordering::Relaxed)
        + REBUILD_DEAD.load(Ordering::Relaxed)
}

// ============================================================================
// The order
// ============================================================================

/// The ordered list of slots. A slot's key is its position: keys change on
/// a relabel, order never does.
pub(super) struct Order {
    keys: Vec<u64>,
    by_key: BTreeMap<u64, u32>,
}

pub(super) static ORDER: RwLock<Order> = RwLock::new(Order {
    keys: Vec::new(),
    by_key: BTreeMap::new(),
});

impl Order {
    fn key(&self, slot: u32) -> u64 {
        self.keys.get(slot as usize).copied().unwrap_or(UNASSIGNED)
    }

    /// The key of `node`'s start slot.
    fn of(&self, node: &NodeCore) -> u64 {
        u32::try_from(node.slot.load(Ordering::Acquire)).map_or(UNASSIGNED, |slot| self.key(slot))
    }

    fn slots(&self) -> usize {
        self.keys.len()
    }

    fn new_slot(&mut self, key: u64) -> u32 {
        let slot = u32::try_from(self.keys.len()).unwrap_or(u32::MAX);
        self.keys.push(key);
        self.by_key.insert(key, slot);
        slot
    }

    /// A new slot right after `after`, or at the front of the list: a root
    /// has no inputs, so it is before every node it will feed.
    fn insert_after(&mut self, after: Option<u32>) -> u32 {
        if self.by_key.is_empty() {
            return self.new_slot(1 << 62);
        }
        loop {
            let lo = after.map_or(0, |slot| self.key(slot));
            let hi = self
                .by_key
                .range(lo.saturating_add(1)..)
                .next()
                .map_or(UNASSIGNED, |(key, _)| *key);
            if lo.checked_add(1).is_some_and(|next| next < hi) {
                return self.new_slot(lo.saturating_add(hi.saturating_sub(lo) / 2));
            }
            self.relabel(if after.is_some() { lo } else { hi });
        }
    }

    /// Spread the keys of the smallest aligned window around `key` whose
    /// density is below the threshold for its size (list labelling): the
    /// window doubles until it is sparse enough, so the cost is amortized
    /// `O(log n)` per insert.
    fn relabel(&mut self, key: u64) {
        RELABELS.fetch_add(1, Ordering::Relaxed);
        for bits in 1..64u32 {
            let size = 1u64 << bits;
            let lo = key & !(size.saturating_sub(1));
            let hi = lo.saturating_add(size);
            let slots: Vec<u32> = self.by_key.range(lo..hi).map(|(_, s)| *s).collect();
            let count = slots.len().saturating_add(2);
            // Density at most 1.5^-bits, the threshold of Bender et al.
            #[allow(clippy::cast_precision_loss, clippy::cast_possible_wrap)]
            let room = (size as f64) * 1.5f64.powi(-(bits as i32));
            #[allow(clippy::cast_precision_loss)]
            let sparse = (count as f64) <= room.max(2.0);
            if !sparse && bits < 63 {
                continue;
            }
            let step = (hi.saturating_sub(lo)) / u64::try_from(count).unwrap_or(u64::MAX);
            // Clear the window first: a new key can equal an old one of a
            // slot not yet moved.
            for slot in &slots {
                self.by_key.remove(&self.key(*slot));
            }
            for (i, slot) in slots.iter().enumerate() {
                let key = lo.saturating_add(
                    step.saturating_mul(u64::try_from(i).unwrap_or(0).saturating_add(1)),
                );
                if let Some(old) = self.keys.get_mut(*slot as usize) {
                    *old = key;
                }
                self.by_key.insert(key, *slot);
            }
            RELABELED_SLOTS.fetch_add(u64::try_from(slots.len()).unwrap_or(0), Ordering::Relaxed);
            return;
        }
    }

    fn covers(&self, spans: &[(u32, u32)], key: u64) -> bool {
        let at = spans.partition_point(|span| self.key(span.1) < key);
        spans.get(at).is_some_and(|span| self.key(span.0) <= key)
    }

    fn covers_all(&self, label: &[(u32, u32)], spans: &[(u32, u32)]) -> bool {
        spans.iter().all(|&(lo, hi)| {
            let (lo, hi) = (self.key(lo), self.key(hi));
            let at = label.partition_point(|span| self.key(span.1) < lo);
            label
                .get(at)
                .is_some_and(|span| self.key(span.0) <= lo && hi <= self.key(span.1))
        })
    }

    /// Add `spans` to `label`; returns how many pairs the label grew by.
    fn merge(&self, label: &mut Vec<(u32, u32)>, spans: &[(u32, u32)]) -> u64 {
        let before = label.len();
        let mut all: Vec<(u32, u32)> = label.iter().chain(spans).copied().collect();
        all.sort_unstable_by_key(|span| self.key(span.0));
        let mut merged: Vec<(u32, u32)> = Vec::with_capacity(all.len());
        for (lo, hi) in all {
            match merged.last_mut() {
                Some(last) if self.key(lo) <= self.key(last.1) => {
                    if self.key(hi) > self.key(last.1) {
                        last.1 = hi;
                    }
                }
                _ => merged.push((lo, hi)),
            }
        }
        *label = merged;
        u64::try_from(label.len().saturating_sub(before)).unwrap_or(u64::MAX)
    }
}

/// A node's reach label: slot pairs covering every node it reaches over the
/// labelled edges, itself included, sorted and disjoint in list order.
#[derive(Default)]
pub(super) struct Label {
    /// The node's own end slot.
    end: u32,
    spans: Vec<(u32, u32)>,
}

fn live(list: &Mutex<Vec<Weak<NodeCore>>>) -> Vec<Arc<NodeCore>> {
    list.lock().iter().filter_map(Weak::upgrade).collect()
}

fn start(node: &NodeCore) -> Option<u32> {
    u32::try_from(node.slot.load(Ordering::Acquire)).ok()
}

// ============================================================================
// The label index
// ============================================================================

struct Index {
    /// Every node that has been given a slot.
    nodes: Vec<Weak<NodeCore>>,
    /// Live nodes after the last prune of `nodes`.
    pruned_at: usize,
    /// Pairs added by non-tree edges since the last compaction.
    overflow: u64,
}

static INDEX: Mutex<Index> = Mutex::new(Index {
    nodes: Vec::new(),
    pruned_at: 0,
    overflow: 0,
});

enum Cause {
    Overflow,
    Dead,
}

/// Index the declared edge `input -> node`. Both endpoints' labels are
/// current when it returns, so a row build that declares the edge holds the
/// row no longer than its own build.
pub(super) fn add_edge(input: &Arc<NodeCore>, node: &Arc<NodeCore>) {
    let mut index = INDEX.lock();
    for end in [input, node] {
        if start(end).is_none() {
            index.nodes.push(Arc::downgrade(end));
        }
    }
    if index.nodes.len() > index.pruned_at.saturating_mul(2).max(1024) {
        index.nodes.retain(|node| node.strong_count() > 0);
        index.pruned_at = index.nodes.len();
    }
    index.place(input, node);
    let live = u64::try_from(index.pruned_at.max(index.nodes.len())).unwrap_or(u64::MAX);
    let cause = if index.overflow > live.saturating_mul(8).max(4096) {
        Some(Cause::Overflow)
    } else if u64::try_from(ORDER.read().slots()).unwrap_or(0) > live.saturating_mul(8).max(1 << 16) {
        Some(Cause::Dead)
    } else {
        None
    };
    if let Some(cause) = cause {
        index.rebuild(&cause);
    }
    #[cfg(debug_assertions)]
    assert_edge(&ORDER.read(), input, node);
}

impl Index {
    /// Give `input -> node` its labels. An edge against the order moves the
    /// nodes `node` reaches before `input` to after it (Pearce and Kelly's
    /// forward window), each right after its latest input's start.
    fn place(&mut self, input: &Arc<NodeCore>, node: &Arc<NodeCore>) {
        let order = ORDER.upgradable_read();
        let order = if start(input).is_none() {
            let mut order = RwLockUpgradableReadGuard::upgrade(order);
            pair(&mut order, input, None);
            parking_lot::RwLockWriteGuard::downgrade_to_upgradable(order)
        } else {
            order
        };
        if start(node).is_none() {
            let mut write = RwLockUpgradableReadGuard::upgrade(order);
            pair(&mut write, node, start(input));
            return;
        }
        if order.of(input) < order.of(node) {
            let mut keep = Vec::new();
            self.propagate(&order, node, &mut keep);
            drop(order);
            drop(keep);
            return;
        }
        // The edge lands against the order: reorder the affected window.
        record_site(&CONFLICT_SITES, input, node);
        let mut keep = Vec::new();
        let mut write = RwLockUpgradableReadGuard::upgrade(order);
        let limit = write.of(input);
        match forward(&write, node, input, limit, &mut keep) {
            None => record_cycle(input, node),
            Some(moved) => {
                REORDERS.fetch_add(1, Ordering::Relaxed);
                REORDERED_NODES
                    .fetch_add(u64::try_from(moved.len()).unwrap_or(u64::MAX), Ordering::Relaxed);
                for m in &moved {
                    let latest = live(&m.inputs)
                        .into_iter()
                        .max_by_key(|input| write.of(input))
                        .unwrap_or_else(|| Arc::clone(input));
                    // The old pair stays in the label, still covering the
                    // nodes that did not move.
                    let old = std::mem::take(&mut m.label.lock().spans);
                    pair(&mut write, m, start(&latest));
                    let mut label = m.label.lock();
                    let mut spans = std::mem::take(&mut label.spans);
                    write.merge(&mut spans, &old);
                    label.spans = spans;
                    drop(label);
                    keep.push(latest);
                }
                // Under the write guard: no reader sees a moved node before
                // its ancestors cover its new place.
                for m in &moved {
                    self.propagate(&write, m, &mut keep);
                }
                keep.extend(moved);
            }
        }
        drop(write);
        drop(keep);
    }

    /// Add `node`'s label to its inputs and their ancestors, up the
    /// labelled edges, stopping where it is already covered. Every node it
    /// visits stays in `keep` until the caller has released `ORDER`.
    fn propagate(&mut self, order: &Order, node: &Arc<NodeCore>, keep: &mut Vec<Arc<NodeCore>>) {
        let spans = node.label.lock().spans.clone();
        let mut seen: FxHashSet<usize> = FxHashSet::default();
        let mut stack = vec![Arc::clone(node)];
        while let Some(below) = stack.pop() {
            let below_key = order.of(&below);
            for input in live(&below.inputs) {
                if order.of(&input) >= below_key || !seen.insert(Arc::as_ptr(&input).addr()) {
                    keep.push(input);
                    continue;
                }
                let mut label = input.label.lock();
                if order.covers_all(&label.spans, &spans) {
                    drop(label);
                    keep.push(input);
                    continue;
                }
                let added = order.merge(&mut label.spans, &spans);
                self.overflow = self.overflow.saturating_add(added);
                drop(label);
                stack.push(input);
            }
            keep.push(below);
        }
    }

    /// Lay the whole list out again from the declared edges: starts in
    /// reverse postorder of a depth-first search, each tree subtree nested
    /// in its root's pair, labels closed over the edges that are not back
    /// edges. A compaction: an order conflict, label overflow and dead slots
    /// call it, never the fire path's steady state.
    fn rebuild(&mut self, cause: &Cause) {
        let started = Instant::now();
        match cause {
            Cause::Overflow => &REBUILD_OVERFLOW,
            Cause::Dead => &REBUILD_DEAD,
        }
        .fetch_add(1, Ordering::Relaxed);
        let nodes: Vec<Arc<NodeCore>> = self.nodes.iter().filter_map(Weak::upgrade).collect();
        self.nodes = nodes.iter().map(Arc::downgrade).collect();
        self.pruned_at = nodes.len();
        let id: FxHashMap<usize, usize> = nodes
            .iter()
            .enumerate()
            .map(|(i, node)| (Arc::as_ptr(node).addr(), i))
            .collect();
        let outs: Vec<Vec<usize>> = nodes
            .iter()
            .map(|node| {
                live(&node.outputs)
                    .iter()
                    .filter_map(|output| id.get(&Arc::as_ptr(output).addr()).copied())
                    .collect()
            })
            .collect();
        let count = nodes.len();
        let (post, tree, back) = depth_first(&outs);
        for &(from, to) in &back {
            if let (Some(from), Some(to)) = (nodes.get(from), nodes.get(to)) {
                record_cycle(from, to);
            }
        }
        let back: FxHashSet<(usize, usize)> = back.into_iter().collect();
        // The slot sequence: a node's start, its tree children's subtrees in
        // reverse visit order, its end. Starts then follow reverse postorder.
        let mut sequence: Vec<(usize, bool)> = Vec::with_capacity(count.saturating_mul(2));
        let mut roots: Vec<usize> = (0..count).filter(|v| tree.get(*v).is_some_and(|t| t.0.is_none())).collect();
        roots.reverse();
        for root in roots {
            let mut stack = vec![(root, false)];
            while let Some((v, done)) = stack.pop() {
                if done {
                    sequence.push((v, false));
                    continue;
                }
                sequence.push((v, true));
                stack.push((v, true));
                // Pushed in visit order, so popped in reverse visit order.
                for &child in tree.get(v).map(|t| t.1.as_slice()).unwrap_or_default() {
                    stack.push((child, false));
                }
            }
        }
        let spacing = (1u64 << 63)
            .checked_div(u64::try_from(sequence.len()).unwrap_or(u64::MAX).saturating_add(1))
            .unwrap_or(1);
        let mut order = Order {
            keys: Vec::with_capacity(sequence.len()),
            by_key: BTreeMap::new(),
        };
        let mut slots = vec![(0u32, 0u32); count];
        for (i, &(v, is_start)) in sequence.iter().enumerate() {
            let key = spacing.saturating_mul(u64::try_from(i).unwrap_or(0).saturating_add(1));
            let slot = order.new_slot(key);
            if let Some(pair) = slots.get_mut(v) {
                if is_start { pair.0 = slot } else { pair.1 = slot }
            }
        }
        let mut labels: Vec<Vec<(u32, u32)>> = vec![Vec::new(); count];
        for &v in &post {
            let mut label = vec![slots.get(v).copied().unwrap_or_default()];
            for &w in outs.get(v).into_iter().flatten() {
                if !back.contains(&(v, w))
                    && let Some(below) = labels.get(w)
                {
                    let below = below.clone();
                    order.merge(&mut label, &below);
                }
            }
            if let Some(slot) = labels.get_mut(v) {
                *slot = label;
            }
        }
        {
            // Readers see the new layout whole. No node drops under the
            // guard: `nodes` holds them all.
            let mut current = ORDER.write();
            *current = order;
            for ((node, spans), (start, end)) in nodes.iter().zip(labels).zip(slots) {
                node.slot.store(u64::from(start), Ordering::Release);
                *node.label.lock() = Label { end, spans };
            }
        }
        self.overflow = 0;
        REBUILD_NANOS.fetch_add(
            u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
        // The edges the layout read; one declared since then is indexed by
        // its own `add_edge`, queued behind the index lock.
        #[cfg(debug_assertions)]
        {
            let order = ORDER.read();
            for (v, node) in nodes.iter().enumerate() {
                for &w in outs.get(v).into_iter().flatten() {
                    if !back.contains(&(v, w))
                        && let Some(output) = nodes.get(w)
                    {
                        assert_edge(&order, node, output);
                    }
                }
            }
        }
    }
}

/// The forward window of Pearce and Kelly for the edge `input -> node` that
/// lands against the order: every node `node` reaches whose start is before
/// `input`'s (key `limit`), in order. `None` when `node` reaches `input`: the
/// edge closes a cycle.
fn forward(
    order: &Order,
    node: &Arc<NodeCore>,
    input: &Arc<NodeCore>,
    limit: u64,
    keep: &mut Vec<Arc<NodeCore>>,
) -> Option<Vec<Arc<NodeCore>>> {
    let mut seen: FxHashSet<usize> = FxHashSet::default();
    seen.insert(Arc::as_ptr(node).addr());
    let mut window = vec![Arc::clone(node)];
    let mut stack = vec![Arc::clone(node)];
    while let Some(at) = stack.pop() {
        let at_key = order.of(&at);
        for output in live(&at.outputs) {
            if Arc::ptr_eq(&output, input) {
                keep.push(output);
                keep.extend(window);
                return None;
            }
            let key = order.of(&output);
            if key <= at_key || key >= limit || !seen.insert(Arc::as_ptr(&output).addr()) {
                keep.push(output);
                continue;
            }
            window.push(Arc::clone(&output));
            stack.push(output);
        }
        keep.push(at);
    }
    window.sort_by_key(|node| order.of(node));
    Some(window)
}

/// Give `node` a fresh pair of slots right after `after` (inside that
/// node's pair), or at the end of the list, and make the pair its label.
fn pair(order: &mut Order, node: &NodeCore, after: Option<u32>) {
    let start = order.insert_after(after);
    let end = order.insert_after(Some(start));
    let mut label = node.label.lock();
    label.end = end;
    let mut spans = std::mem::take(&mut label.spans);
    order.merge(&mut spans, &[(start, end)]);
    label.spans = spans;
    drop(label);
    node.slot.store(u64::from(start), Ordering::Release);
}

/// A depth-first search over `outs`: the postorder, each node's tree parent
/// and tree children in visit order, and the back edges.
#[allow(clippy::type_complexity)]
fn depth_first(
    outs: &[Vec<usize>],
) -> (Vec<usize>, Vec<(Option<usize>, Vec<usize>)>, Vec<(usize, usize)>) {
    const WHITE: u8 = 0;
    const GRAY: u8 = 1;
    const BLACK: u8 = 2;
    let count = outs.len();
    let mut color = vec![WHITE; count];
    let mut tree: Vec<(Option<usize>, Vec<usize>)> = vec![(None, Vec::new()); count];
    let mut post = Vec::with_capacity(count);
    let mut back = Vec::new();
    for root in 0..count {
        if color.get(root) != Some(&WHITE) {
            continue;
        }
        let mut stack: Vec<(usize, usize)> = vec![(root, 0)];
        if let Some(c) = color.get_mut(root) {
            *c = GRAY;
        }
        while let Some(top) = stack.last_mut() {
            let (v, next) = *top;
            if let Some(&w) = outs.get(v).and_then(|o| o.get(next)) {
                top.1 = next.saturating_add(1);
                match color.get(w).copied() {
                    Some(WHITE) => {
                        if let Some(c) = color.get_mut(w) {
                            *c = GRAY;
                        }
                        if let Some(t) = tree.get_mut(w) {
                            t.0 = Some(v);
                        }
                        if let Some(t) = tree.get_mut(v) {
                            t.1.push(w);
                        }
                        stack.push((w, 0));
                    }
                    Some(GRAY) => back.push((v, w)),
                    _ => {}
                }
                continue;
            }
            stack.pop();
            if let Some(c) = color.get_mut(v) {
                *c = BLACK;
            }
            post.push(v);
        }
    }
    (post, tree, back)
}

/// Record a back edge `from -> to` that the labels leave out.
fn record_cycle(from: &NodeCore, to: &NodeCore) {
    CYCLES.fetch_add(1, Ordering::Relaxed);
    record_site(&CYCLE_SITES, from, to);
}

/// Count the edge `from -> to` in `sites`, by creation sites.
fn record_site(sites: &Sites, from: &NodeCore, to: &NodeCore) {
    let site = |node: &NodeCore| {
        node.caller
            .get()
            .map_or_else(|| "<unknown>".to_owned(), ToString::to_string)
    };
    sites
        .lock()
        .entry((site(from), site(to)))
        .and_modify(|count| *count = count.saturating_add(1))
        .or_insert(1);
}

fn ranked(sites: &Sites) -> Vec<((String, String), u64)> {
    let mut ranked: Vec<_> = sites
        .lock()
        .iter()
        .map(|(key, count)| (key.clone(), *count))
        .collect();
    ranked.sort_by_key(|entry| std::cmp::Reverse(entry.1));
    ranked
}

/// Edges that landed against the order, most frequent first.
pub(super) fn conflict_sites() -> Vec<((String, String), u64)> {
    ranked(&CONFLICT_SITES)
}

/// Back edges the labels leave out, most frequent first.
pub(super) fn cycle_sites() -> Vec<((String, String), u64)> {
    ranked(&CYCLE_SITES)
}

/// The labels cover the declared edge `input -> node`: the input is first in
/// the order and its label holds the node's.
#[cfg(debug_assertions)]
fn assert_edge(order: &Order, input: &NodeCore, node: &NodeCore) {
    let (ki, kn) = (order.of(input), order.of(node));
    if ki >= kn {
        // A back edge, left out of the labels by the last compaction.
        return;
    }
    let spans = node.label.lock().spans.clone();
    let label = input.label.lock();
    assert!(
        order.covers(&label.spans, kn) && order.covers_all(&label.spans, &spans),
        "hyphae: reach index: the label of an input does not cover its output"
    );
}

// ============================================================================
// The frontier
// ============================================================================

#[derive(Default)]
pub(super) struct Frontier {
    tickets: BTreeMap<u64, Vec<Weak<NodeCore>>>,
    entries: u64,
}

pub(super) static FRONTIER: Mutex<Frontier> = Mutex::new(Frontier {
    tickets: BTreeMap::new(),
    entries: 0,
});

/// `node` now holds `ticket` pending. Called under the node's pending lock,
/// so the entry is in place before the pusher's own entry can leave.
pub(super) fn hold(node: &Weak<NodeCore>, ticket: u64) {
    let mut frontier = FRONTIER.lock();
    frontier.tickets.entry(ticket).or_default().push(Weak::clone(node));
    frontier.entries = frontier.entries.saturating_add(1);
    MAX_FRONTIER.fetch_max(frontier.entries, Ordering::Relaxed);
}

/// The node at `address` no longer holds `ticket`.
pub(super) fn release(address: usize, ticket: u64) {
    let mut frontier = FRONTIER.lock();
    let Some(nodes) = frontier.tickets.get_mut(&ticket) else {
        return;
    };
    let Some(at) = nodes.iter().position(|node| node.as_ptr().addr() == address) else {
        return;
    };
    nodes.swap_remove(at);
    if nodes.is_empty() {
        frontier.tickets.remove(&ticket);
    }
    frontier.entries = frontier.entries.saturating_sub(1);
}

/// A node holding a pending entry at or below `ticket` that can reach
/// `node`, either along the labels or into one of `gates`, the sources of
/// `node`'s gated edges open at `ticket`. `None` when `node` may fire.
pub(super) fn blocker(
    node: &NodeCore,
    ticket: u64,
    gates: &[Arc<NodeCore>],
) -> Option<Arc<NodeCore>> {
    let address = std::ptr::from_ref(node).addr();
    let (mut queries, mut scanned) = (0u64, 0u64);
    // Nodes upgraded during the scan drop after the lock is released: a
    // node's drop takes it.
    let mut keep: Vec<Arc<NodeCore>> = Vec::new();
    let found = {
        let frontier = FRONTIER.lock();
        let order = ORDER.read();
        let target = order.of(node);
        let gate_keys: Vec<(usize, u64)> = gates
            .iter()
            .map(|gate| (Arc::as_ptr(gate).addr(), order.of(gate)))
            .collect();
        let mut found = None;
        'scan: for nodes in frontier.tickets.range(..=ticket).map(|(_, nodes)| nodes) {
            for held in nodes {
                scanned = scanned.saturating_add(1);
                let at = held.as_ptr().addr();
                if at == address {
                    continue;
                }
                let Some(held) = held.upgrade() else { continue };
                let from = order.of(&held);
                let mut reaches = |to: u64| {
                    from < to && {
                        queries = queries.saturating_add(1);
                        order.covers(&held.label.lock().spans, to)
                    }
                };
                if reaches(target)
                    || gate_keys.iter().any(|&(gate, to)| gate == at || reaches(to))
                {
                    found = Some(held);
                    break 'scan;
                }
                keep.push(held);
            }
        }
        drop(order);
        drop(frontier);
        found
    };
    drop(keep);
    LABEL_QUERIES.fetch_add(queries, Ordering::Relaxed);
    FRONTIER_SCANNED.fetch_add(scanned, Ordering::Relaxed);
    found
}

/// The model's readiness rule, against the declared edges instead of the
/// labels: no node holding an entry at or below `ticket` reaches `node`, or
/// one of its open `gates`, along edges that go up in key order.
#[cfg(debug_assertions)]
pub(super) fn assert_ready(node: &Arc<NodeCore>, ticket: u64, gates: &[Arc<NodeCore>]) {
    let held: Vec<Arc<NodeCore>> = FRONTIER
        .lock()
        .tickets
        .range(..=ticket)
        .flat_map(|(_, nodes)| nodes.iter().filter_map(Weak::upgrade))
        .collect();
    let mut targets: FxHashSet<usize> = gates.iter().map(|g| Arc::as_ptr(g).addr()).collect();
    targets.insert(Arc::as_ptr(node).addr());
    for from in held {
        if Arc::ptr_eq(&from, node) {
            continue;
        }
        let mut seen: FxHashSet<usize> = FxHashSet::default();
        let mut stack = vec![Arc::clone(&from)];
        let starts_at_gate = targets.contains(&Arc::as_ptr(&from).addr());
        assert!(
            !starts_at_gate,
            "hyphae: readiness rule: ticket {ticket} fired while an open gated input holds an entry at or below it"
        );
        while let Some(at) = stack.pop() {
            let outputs = live(&at.outputs);
            let order = ORDER.read();
            let ahead: Vec<bool> = outputs.iter().map(|o| order.of(&at) < order.of(o)).collect();
            drop(order);
            for (output, ahead) in outputs.into_iter().zip(ahead) {
                if !ahead || !seen.insert(Arc::as_ptr(&output).addr()) {
                    continue;
                }
                assert!(
                    !targets.contains(&Arc::as_ptr(&output).addr()),
                    "hyphae: readiness rule: ticket {ticket} fired while a node that reaches it holds an entry at or below it"
                );
                stack.push(output);
            }
        }
    }
}
