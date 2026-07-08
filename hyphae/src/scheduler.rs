//! Opt-in propagation scheduler — deferred, ordered, glitch-free flushes under
//! an explicit [`batch`] boundary.
//!
//! Gated behind the `scheduler` feature. When the feature is off, or when code
//! runs outside a [`batch`], propagation takes hyphae's exact synchronous
//! eager-push path — this module changes nothing about the default.
//!
//! # Model
//!
//! Outside a batch the tick context is inactive and [`Cell::notify`] runs the
//! synchronous cascade. Inside [`batch`], each `notify` instead **enqueues** a
//! deferred `(write_value, fanout)` op keyed by cell id, at the cell's
//! **height** (`1 + max(dep.height)`; sources are 0). Enqueues **coalesce**
//! last-write-wins per cell within the tick. At the closing brace the queue
//! **drains in non-decreasing height order**, settling each cell's value then
//! running its fanout (which enqueues its subscribers at strictly greater
//! heights).
//!
//! So every height-`k` cell settles before any height-`k+1` subscriber runs,
//! and a multi-input node (a diamond's join) is popped **once**, after every
//! lower input has coalesced into it — it emits once per tick with the settled
//! value instead of once per input arrival. That is the glitch-freedom: the
//! redundant re-fires a synchronous diamond makes are collapsed.
//!
//! # Scope (Phase 0)
//!
//! Coalescing is last-write-wins, which is correct for **behavior** cells
//! (`map`/`filter`/`join`/`switch_map` — latest-value semantics, exactly where
//! the glitch lives). It is **not** correct for **event** operators
//! (`scan`/`pairwise`/`buffer`/`zip`/`merge`), where dropping an intermediate
//! value changes the result; scoping the opt-in away from those is the next
//! phase. Height is memoized per tick (topology is assumed stable within a
//! tick); a persistent height cache with topology-epoch invalidation, and
//! recompute-at-pop for switch_map rewiring mid-drain, are later phases too.
//!
//! Because a batch defers value settlement to the drain, a cell read *inside*
//! the batch (before the closing brace) still sees its pre-batch value — the
//! glitch-free trade-off, and the reason this is opt-in.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

use rustc_hash::{FxHashMap, FxHashSet};
use uuid::Uuid;

use crate::traits::DepNode;

thread_local! {
    static TICK: RefCell<Tick> = RefCell::new(Tick::new());
}

/// Global topology epoch. Bumped on every edge change (`Cell::own`,
/// `Cell::own_keyed`, `SubscriptionGuard::drop`) so cached cell heights, tagged
/// with the epoch they were computed under, are invalidated lazily on the next
/// read. Starts at 1 so a zero-initialized `height_cache` reads as stale.
static TOPOLOGY_EPOCH: AtomicU64 = AtomicU64::new(1);

/// Invalidate all cached heights by advancing the topology epoch. Cheap enough
/// to call unconditionally on any edge change.
pub(crate) fn bump_topology_epoch() {
    TOPOLOGY_EPOCH.fetch_add(1, Ordering::Relaxed);
}

/// The current epoch, truncated to the 32 bits packed into a height cache. Wraps
/// only after ~4 billion topology changes, which cannot alias a live cache entry
/// in practice.
fn current_epoch() -> u32 {
    TOPOLOGY_EPOCH.load(Ordering::Relaxed) as u32
}

/// Per-thread propagation tick: the deferred frontier plus the reentrancy depth
/// and a per-tick height memo.
struct Tick {
    /// Reentrancy depth. `> 0` means a batch is active on this thread and
    /// `notify` should defer. Nested `batch` calls join the outermost tick.
    depth: u32,
    /// Height-ordered frontier: `(height, id) -> deferred op`. `BTreeMap`
    /// pops the minimum `(height, id)` first, giving the height ordering.
    order: BTreeMap<(u64, Uuid), Box<dyn FnOnce()>>,
    /// `id -> its current height key in `order``, so a re-notify can find and
    /// drop the cell's previously-queued op (coalescing) before re-inserting.
    scheduled: FxHashMap<Uuid, u64>,
}

impl Tick {
    fn new() -> Self {
        Self {
            depth: 0,
            order: BTreeMap::new(),
            scheduled: FxHashMap::default(),
        }
    }

    /// Enqueue (or coalesce) `run` for cell `id` at `height`. If the cell is
    /// already queued this tick, its previous op is dropped (last-write-wins).
    fn enqueue(&mut self, id: Uuid, height: u64, run: Box<dyn FnOnce()>) {
        if let Some(prev_height) = self.scheduled.insert(id, height) {
            // Drop the superseded op (and the value/cell it captured). We hold
            // no cell lock here, so a cascading Arc drop is safe.
            self.order.remove(&(prev_height, id));
        }
        self.order.insert((height, id), run);
    }

    /// Remove and return the minimum-height op, or `None` when the frontier is
    /// empty.
    fn pop_min(&mut self) -> Option<Box<dyn FnOnce()>> {
        let ((_, id), run) = self.order.pop_first()?;
        self.scheduled.remove(&id);
        Some(run)
    }

    /// Discard all per-tick state. Called when the outermost batch exits (or
    /// unwinds), so a panicked drain leaves a clean slate for the next batch.
    fn clear(&mut self) {
        self.order.clear();
        self.scheduled.clear();
    }
}

/// The cell's propagation height, `1 + max(dep.height)` (sources are 0), read
/// from its persistent per-node cache when the cache is current for `epoch` and
/// recomputed (and re-cached) otherwise. In a stable topology this is one atomic
/// load; `deps()` is walked only on the first read after an edge change.
fn compute_height(node: &dyn DepNode) -> u64 {
    let mut stack = FxHashSet::default();
    height_dfs(node, current_epoch(), &mut stack)
}

/// Height DFS backing [`compute_height`]. Uses each node's [`DepNode::height_cache`]
/// as an epoch-tagged memo (so results persist across ticks and across the
/// recursion). `stack` breaks dependency cycles — a back-edge to a node already
/// on the current path contributes height 0 rather than recursing forever.
fn height_dfs(node: &dyn DepNode, epoch: u32, stack: &mut FxHashSet<Uuid>) -> u64 {
    if let Some(cache) = node.height_cache() {
        let packed = cache.load(Ordering::Relaxed);
        if (packed >> 32) as u32 == epoch {
            return packed & 0xFFFF_FFFF;
        }
    }
    let id = node.id();
    if !stack.insert(id) {
        return 0;
    }
    let mut height = 0u64;
    for dep in node.deps() {
        height = height.max(height_dfs(dep.as_ref(), epoch, stack) + 1);
    }
    stack.remove(&id);
    if let Some(cache) = node.height_cache() {
        cache.store(((epoch as u64) << 32) | height, Ordering::Relaxed);
    }
    height
}

/// Whether a batch is active on the current thread. Cheap — one thread-local
/// borrow and an integer compare. This is the single check `notify` pays on the
/// synchronous hot path when the `scheduler` feature is on but no batch is open.
pub(crate) fn tick_active() -> bool {
    TICK.with(|t| t.borrow().depth > 0)
}

/// Enqueue a deferred propagation op for cell `id`, computing its height from
/// `node`. Called by `notify` only when [`tick_active`] is already true.
pub(crate) fn enqueue(id: Uuid, node: &dyn DepNode, run: impl FnOnce() + 'static) {
    let height = compute_height(node);
    TICK.with(|t| t.borrow_mut().enqueue(id, height, Box::new(run)));
}

/// Drain the frontier to fixpoint in height order. Runs each op *outside* the
/// thread-local borrow, because the op's fanout re-enters [`enqueue`] for its
/// subscribers. The depth stays raised across the drain, so those re-entrant
/// notifies defer too (rather than cascading synchronously).
fn drain() {
    while let Some(run) = TICK.with(|t| t.borrow_mut().pop_min()) {
        run();
    }
}

/// Decrements the tick depth on scope exit (including unwind), and clears the
/// tick when the outermost batch closes.
struct DepthGuard;

impl Drop for DepthGuard {
    fn drop(&mut self) {
        TICK.with(|t| {
            let mut tick = t.borrow_mut();
            tick.depth -= 1;
            if tick.depth == 0 {
                tick.clear();
            }
        });
    }
}

/// Run `f` as a single propagation batch.
///
/// Every `set`/`notify` inside `f` enqueues instead of cascading; the whole
/// downstream DAG then flushes once, in height order, glitch-free, at the
/// closing brace. Nested `batch` calls join the outermost tick — only the
/// outermost drains.
///
/// Outside the `scheduler` feature this is a behavior-preserving passthrough;
/// see the module docs for the deferred semantics when the feature is on.
pub fn batch<R>(f: impl FnOnce() -> R) -> R {
    let outermost = TICK.with(|t| {
        let mut tick = t.borrow_mut();
        tick.depth += 1;
        tick.depth == 1
    });
    // Guard decrements depth even if `f` or the drain unwinds.
    let _guard = DepthGuard;
    let result = f();
    if outermost {
        // Drain while depth is still raised, so fanout-triggered notifies enqueue.
        drain();
    }
    result
}
