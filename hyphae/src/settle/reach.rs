//! Readiness from reachability (`docs/settle-epochs-model/reach_model.rs`).
//!
//! Node `n` fires its lowest pending ticket `t` when no pending entry
//! `u <= t` at another node can reach `n`. Two structures answer it:
//!
//! - **The label index.** Every node has a key in a topological order of the
//!   declared edges, and a label: sorted key spans that cover every node it
//!   reaches. Keys are allocated tree-cover style: a new node takes half of
//!   its first input's free key space, so the input's span, and through it
//!   every ancestor's label, already covers it. Every other edge adds the
//!   target's label to the source and to the source's ancestors, eagerly. An
//!   edge into a node whose key is not above the input's re-keys the node
//!   into the gap between its inputs and outputs (a family row built after
//!   its reader). When no gap or no free space is left, or the spans added by
//!   non-tree edges grow, the index is rebuilt from the declared edges.
//!   Labels are never shrunk when a node or edge goes away: a stale span only
//!   adds waiting.
//! - **The frontier.** For each ticket in flight, the nodes that hold it
//!   pending. A readiness check scans the entries at or below its ticket and
//!   rejects most by key order: `f` cannot reach `n` when `key(f) >= key(n)`.
//!
//! A gated edge `src -> n` (a family into its reader) is not in the labels.
//! It counts at ticket `t` only while its gate's delivery of `t` is pending
//! at `n`, which is when `n` is the node asking: the caller passes the open
//! gates, and an entry at `src`, or one that reaches `src`, blocks.

use std::{
    collections::BTreeMap,
    sync::{
        Arc, LazyLock, Weak,
        atomic::{AtomicU64, Ordering},
    },
};

use parking_lot::Mutex;
use rustc_hash::{FxHashMap, FxHashSet};

use super::NodeCore;

/// The key of a node no declared edge has touched. Nothing reaches it.
pub(super) const UNASSIGNED: u64 = u64::MAX;

/// Root nodes take key chunks from the upper half; a rebuild lays every
/// node out in the lower half.
const ROOT_BASE: u64 = 1 << 63;
const ROOT_CHUNK: u64 = 1 << 32;

pub(super) static LABEL_QUERIES: AtomicU64 = AtomicU64::new(0);
pub(super) static FRONTIER_SCANNED: AtomicU64 = AtomicU64::new(0);
pub(super) static MAX_FRONTIER: AtomicU64 = AtomicU64::new(0);
pub(super) static REBUILDS: AtomicU64 = AtomicU64::new(0);
pub(super) static CYCLES: AtomicU64 = AtomicU64::new(0);
/// Back edges a rebuild left out of the labels, `(from site, to site) -> count`.
static CYCLE_SITES: LazyLock<Mutex<FxHashMap<(String, String), u64>>> =
    LazyLock::new(Mutex::default);

/// A node's reach label.
#[derive(Default)]
pub(super) struct Label {
    /// Keys `[lo, hi)` not yet given to a tree child.
    free: (u64, u64),
    /// Sorted, disjoint key spans `[lo, hi)` covering every node this one
    /// reaches over the labelled edges, itself included.
    spans: Vec<(u64, u64)>,
}

impl Label {
    fn covers(&self, key: u64) -> bool {
        let at = self.spans.partition_point(|span| span.1 <= key);
        self.spans.get(at).is_some_and(|span| span.0 <= key)
    }

    fn covers_all(&self, spans: &[(u64, u64)]) -> bool {
        spans.iter().all(|&(lo, hi)| {
            let at = self.spans.partition_point(|span| span.1 <= lo);
            self.spans
                .get(at)
                .is_some_and(|span| span.0 <= lo && hi <= span.1)
        })
    }

    /// Add `spans`; returns how many spans the label grew by.
    fn merge(&mut self, spans: &[(u64, u64)]) -> u64 {
        let before = self.spans.len();
        let mut all: Vec<(u64, u64)> = self.spans.iter().chain(spans).copied().collect();
        all.sort_unstable();
        let mut merged: Vec<(u64, u64)> = Vec::with_capacity(all.len());
        for (lo, hi) in all {
            match merged.last_mut() {
                Some(last) if lo <= last.1 => last.1 = last.1.max(hi),
                _ => merged.push((lo, hi)),
            }
        }
        self.spans = merged;
        u64::try_from(self.spans.len().saturating_sub(before)).unwrap_or(u64::MAX)
    }
}

fn key(node: &NodeCore) -> u64 {
    node.key.load(Ordering::Acquire)
}

fn live(list: &Mutex<Vec<Weak<NodeCore>>>) -> Vec<Arc<NodeCore>> {
    list.lock().iter().filter_map(Weak::upgrade).collect()
}

// ============================================================================
// The label index
// ============================================================================

struct Index {
    /// Every node that has been given a key.
    nodes: Vec<Weak<NodeCore>>,
    /// Live nodes after the last prune of `nodes`.
    pruned_at: usize,
    next_root: u64,
    /// Spans added by non-tree edges since the last rebuild.
    overflow: u64,
    /// The overflow that triggers a rebuild.
    limit: u64,
}

static INDEX: Mutex<Index> = Mutex::new(Index {
    nodes: Vec::new(),
    pruned_at: 0,
    next_root: ROOT_BASE,
    overflow: 0,
    limit: 4096,
});

/// Index the declared edge `input -> node`. Both endpoints' labels are
/// current when it returns, so a row build that declares the edge holds the
/// row no longer than its own build.
pub(super) fn add_edge(input: &Arc<NodeCore>, node: &Arc<NodeCore>) {
    let mut index = INDEX.lock();
    for end in [input, node] {
        if key(end) == UNASSIGNED {
            index.nodes.push(Arc::downgrade(end));
        }
    }
    if index.nodes.len() > index.pruned_at.saturating_mul(2).max(1024) {
        index.nodes.retain(|node| node.strong_count() > 0);
        index.pruned_at = index.nodes.len();
    }
    if !index.place(input, node) || index.overflow > index.limit {
        index.rebuild();
    }
    #[cfg(debug_assertions)]
    assert_edge(input, node);
}

impl Index {
    /// Give `input -> node` its labels without a rebuild. False when there
    /// is no key space for it.
    fn place(&mut self, input: &Arc<NodeCore>, node: &Arc<NodeCore>) -> bool {
        if key(input) == UNASSIGNED && !self.root(input) {
            return false;
        }
        if key(node) == UNASSIGNED {
            return child(input, node);
        }
        if key(input) < key(node) {
            self.propagate(node, &mut Vec::new());
            return true;
        }
        self.rekey(node)
    }

    fn root(&mut self, node: &NodeCore) -> bool {
        let lo = self.next_root;
        let Some(hi) = lo.checked_add(ROOT_CHUNK).filter(|hi| *hi < UNASSIGNED) else {
            return false;
        };
        self.next_root = hi;
        *node.label.lock() = Label {
            free: (lo.saturating_add(1), hi),
            spans: vec![(lo, hi)],
        };
        node.key.store(lo, Ordering::Release);
        true
    }

    /// Add `node`'s label to its inputs and their ancestors, up the
    /// labelled edges, stopping where it is already covered.
    ///
    /// Every node it visits stays in `keep`: a caller holding the frontier
    /// lock drops them after it, since a node's drop takes that lock.
    fn propagate(&mut self, node: &Arc<NodeCore>, keep: &mut Vec<Arc<NodeCore>>) {
        let spans = node.label.lock().spans.clone();
        let mut seen: FxHashSet<usize> = FxHashSet::default();
        let mut stack = vec![Arc::clone(node)];
        while let Some(below) = stack.pop() {
            let below_key = key(&below);
            for input in live(&below.inputs) {
                if key(&input) >= below_key || !seen.insert(Arc::as_ptr(&input).addr()) {
                    continue;
                }
                let mut label = input.label.lock();
                if label.covers_all(&spans) {
                    continue;
                }
                self.overflow = self.overflow.saturating_add(label.merge(&spans));
                drop(label);
                stack.push(input);
            }
            keep.push(below);
        }
    }

    /// Move `node` into the gap between its inputs and its outputs, and
    /// label its inputs with it. False when there is no gap.
    fn rekey(&mut self, node: &Arc<NodeCore>) -> bool {
        let lo = live(&node.inputs).iter().map(|input| key(input)).max().unwrap_or(0);
        let hi = live(&node.outputs)
            .iter()
            .map(|output| key(output))
            .min()
            .unwrap_or(UNASSIGNED);
        if hi.saturating_sub(lo) < 2 {
            return false;
        }
        let at = lo.saturating_add(hi.saturating_sub(lo) / 2);
        let mut keep = Vec::new();
        // Readers compare keys and labels: they see the move whole.
        let frontier = FRONTIER.lock();
        {
            let mut label = node.label.lock();
            label.merge(&[(at, at.saturating_add(1))]);
            label.free = (at, at);
        }
        node.key.store(at, Ordering::Release);
        self.propagate(node, &mut keep);
        drop(frontier);
        drop(keep);
        true
    }

    /// Lay out every node again from the declared edges: keys in reverse
    /// postorder of a depth-first search, so each tree subtree is one span,
    /// and labels closed over the edges that are not back edges.
    fn rebuild(&mut self) {
        REBUILDS.fetch_add(1, Ordering::Relaxed);
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
        let (post, tree_size, back) = depth_first(&outs);
        for &(from, to) in &back {
            if let (Some(from), Some(to)) = (nodes.get(from), nodes.get(to)) {
                record_cycle(from, to);
            }
        }
        let back: FxHashSet<(usize, usize)> = back.into_iter().collect();
        let spacing = ROOT_BASE
            .checked_div(u64::try_from(count).unwrap_or(u64::MAX).saturating_add(1))
            .unwrap_or(1);
        let mut rank = vec![0u64; count];
        for (i, &v) in post.iter().enumerate() {
            if let Some(slot) = rank.get_mut(v) {
                *slot = u64::try_from(count.saturating_sub(1).saturating_sub(i)).unwrap_or(0);
            }
        }
        let mut labels: Vec<Label> = (0..count).map(|_| Label::default()).collect();
        for &v in &post {
            let lo = rank.get(v).copied().unwrap_or(0).saturating_mul(spacing);
            let size = tree_size.get(v).copied().unwrap_or(1);
            let mut label = Label {
                free: (lo.saturating_add(1), lo.saturating_add(spacing)),
                spans: vec![(lo, lo.saturating_add(size.saturating_mul(spacing)))],
            };
            for &w in outs.get(v).into_iter().flatten() {
                if !back.contains(&(v, w))
                    && let Some(below) = labels.get(w)
                {
                    let spans = below.spans.clone();
                    label.merge(&spans);
                }
            }
            if let Some(slot) = labels.get_mut(v) {
                *slot = label;
            }
        }
        // Readers see the new layout whole. No node drops under the lock:
        // `nodes` holds them all, and a node's drop takes it.
        let frontier = FRONTIER.lock();
        for ((node, label), rank) in nodes.iter().zip(labels).zip(rank) {
            node.key.store(rank.saturating_mul(spacing), Ordering::Release);
            *node.label.lock() = label;
        }
        drop(frontier);
        self.next_root = ROOT_BASE;
        self.overflow = 0;
        self.limit = u64::try_from(count).unwrap_or(u64::MAX).saturating_mul(8).max(4096);
        #[cfg(debug_assertions)]
        for node in &nodes {
            // An output whose own `add_edge` is still waiting for the index
            // has no key yet; that call indexes the edge.
            for output in live(&node.outputs) {
                if key(node) < key(&output) && key(&output) != UNASSIGNED {
                    assert_edge(node, &output);
                }
            }
        }
    }
}

/// Give `node`, which has no key, half of `parent`'s free key space.
fn child(parent: &NodeCore, node: &NodeCore) -> bool {
    let mut label = parent.label.lock();
    let (lo, hi) = label.free;
    let half = hi.saturating_sub(lo) / 2;
    if half < 2 {
        return false;
    }
    let mid = lo.saturating_add(half);
    label.free = (mid, hi);
    drop(label);
    *node.label.lock() = Label {
        free: (lo.saturating_add(1), mid),
        spans: vec![(lo, mid)],
    };
    node.key.store(lo, Ordering::Release);
    true
}

/// A depth-first search over `outs`: the postorder, each node's tree subtree
/// size, and the back edges.
fn depth_first(outs: &[Vec<usize>]) -> (Vec<usize>, Vec<u64>, Vec<(usize, usize)>) {
    const WHITE: u8 = 0;
    const GRAY: u8 = 1;
    const BLACK: u8 = 2;
    let count = outs.len();
    let mut color = vec![WHITE; count];
    let mut size = vec![1u64; count];
    let mut post = Vec::with_capacity(count);
    let mut back = Vec::new();
    for start in 0..count {
        if color.get(start) != Some(&WHITE) {
            continue;
        }
        let mut stack: Vec<(usize, usize, Option<usize>)> = vec![(start, 0, None)];
        if let Some(c) = color.get_mut(start) {
            *c = GRAY;
        }
        while let Some(top) = stack.last_mut() {
            let (v, next, parent) = *top;
            if let Some(&w) = outs.get(v).and_then(|o| o.get(next)) {
                top.1 = next.saturating_add(1);
                match color.get(w).copied() {
                    Some(WHITE) => {
                        if let Some(c) = color.get_mut(w) {
                            *c = GRAY;
                        }
                        stack.push((w, 0, Some(v)));
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
            if let Some(parent) = parent {
                let grown = size.get(v).copied().unwrap_or(1);
                if let Some(s) = size.get_mut(parent) {
                    *s = s.saturating_add(grown);
                }
            }
        }
    }
    (post, size, back)
}

/// Record a back edge `from -> to` that the labels leave out.
fn record_cycle(from: &NodeCore, to: &NodeCore) {
    CYCLES.fetch_add(1, Ordering::Relaxed);
    let site = |node: &NodeCore| {
        node.caller
            .get()
            .map_or_else(|| "<unknown>".to_owned(), ToString::to_string)
    };
    CYCLE_SITES
        .lock()
        .entry((site(from), site(to)))
        .and_modify(|count| *count = count.saturating_add(1))
        .or_insert(1);
}

/// Back edges the labels leave out, most frequent first.
pub(super) fn cycle_sites() -> Vec<((String, String), u64)> {
    let mut sites: Vec<_> = CYCLE_SITES
        .lock()
        .iter()
        .map(|(key, count)| (key.clone(), *count))
        .collect();
    sites.sort_by_key(|entry| std::cmp::Reverse(entry.1));
    sites
}

/// The labels cover the declared edge `input -> node`: the input is first in
/// key order and its label holds the node's.
#[cfg(debug_assertions)]
fn assert_edge(input: &NodeCore, node: &NodeCore) {
    let (ki, kn) = (key(input), key(node));
    if ki >= kn {
        // A back edge, left out of the labels by the last rebuild.
        return;
    }
    let spans = node.label.lock().spans.clone();
    let label = input.label.lock();
    assert!(
        label.covers(kn) && label.covers_all(&spans),
        "hyphae: reach index: the label of an input (key {ki}, {:?}) does not cover its \
         output (key {kn}, {spans:?})",
        label.spans
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
        let target = key(node);
        let gate_keys: Vec<(usize, u64)> = gates
            .iter()
            .map(|gate| (Arc::as_ptr(gate).addr(), key(gate)))
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
                let from = key(&held);
                let mut reaches = |to: u64| {
                    from < to && {
                        queries = queries.saturating_add(1);
                        held.label.lock().covers(to)
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
            for output in live(&at.outputs) {
                if key(&at) >= key(&output) || !seen.insert(Arc::as_ptr(&output).addr()) {
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
