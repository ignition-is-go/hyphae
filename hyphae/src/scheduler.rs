//! Opt-in propagation scheduler — deferred, ordered, glitch-free flushes under
//! an explicit [`batch`] boundary.
//!
//! Gated behind the `scheduler` feature. When the feature is off, or when code
//! runs outside a [`batch`], propagation takes hyphae's exact synchronous
//! eager-push path — this module changes nothing about the default.
//!
//! # Model
//!
//! Outside a batch the tick is inactive and [`Cell::notify`] runs the
//! synchronous cascade. Inside [`batch`], each `notify` instead **enqueues** a
//! deferred `(write_value, fanout)` op keyed by cell id, at the cell's
//! **height** (`1 + max(dep.height)`; sources are 0). Enqueues **coalesce**
//! last-write-wins per cell within the tick. The queue **drains in
//! non-decreasing height order**, settling each cell's value then running its
//! fanout (which enqueues its subscribers at strictly greater heights).
//!
//! So every height-`k` cell settles before any height-`k+1` subscriber runs,
//! and a multi-input node (a diamond's join) is popped **once**, after every
//! lower input has coalesced into it — it emits once per tick with the settled
//! value instead of once per input arrival. That is the glitch-freedom: the
//! redundant re-fires a synchronous diamond makes are collapsed.
//!
//! # Cross-thread model
//!
//! The tick queue is process-wide, not per-thread. This is load-bearing, not
//! an implementation detail: hyphae ships timer/interval constructors as the
//! documented way to build `.batched()` fan-out sources (clock/interval ticks
//! feeding a wide reactive graph — exactly the case `.batched()`'s own docs
//! recommend it for), and those timers, plus ordinary application code
//! running on an async runtime's worker pool, routinely touch overlapping
//! cell graphs from different threads. A per-thread tick queue gives each
//! thread its own "exactly once, glitch-free" guarantee in isolation, but two
//! threads' batches converging on a shared downstream cell aren't coordinated
//! with each other at all: a genuine value change from one thread's batch can
//! be silently overwritten by the other's before any subscriber observes it,
//! with no error — just updates that stop arriving, intermittently, depending
//! on how the OS scheduled the two threads that tick. That failure mode is
//! *worse* than the synchronous eager path's redundant re-fires, so the
//! scheduler serializes every thread's deferred ops through one shared queue
//! instead of promising a guarantee it can only keep within a single thread.
//!
//! Concretely: [`enqueue`] and [`drain`] share one `Mutex`-protected queue.
//! Deferral is gated by `depth > 0 || draining` (a `batch()` open anywhere,
//! or a drain in flight). Draining is **claimed at a thread's outermost
//! `batch()` close** — the close that brings this thread's [`BATCH_NEST`]
//! back to 0 — by whichever such close finds `draining` false and the queue
//! non-empty; it sets `draining` (the single-drainer mutex) and drains the
//! queue empty in short bursts, then clears `draining` the instant the queue
//! runs dry. Crucially there is **no park and no "wait for the count to
//! quiesce" loop**: a thread never pins itself as a persistent drainer, so a
//! peer that keeps opening batches can't starve it. Draining hands off across
//! close boundaries instead — an op enqueued after one drain releases is
//! drained by the next outermost close that observes pending work. This is
//! strand-free because a drain's empty-check and its `draining = false`
//! release happen together under the lock: an op enqueued before that release
//! is popped by the drain; one enqueued after is seen by the next close. The
//! lock is never held while running a popped op or a `batch()`'s `f()` — both
//! may re-enter `enqueue`/`batch` for their own fanout/nesting, which would
//! deadlock against a self-held lock otherwise.
//!
//! This means `batch()`'s "settled by the time this call returns" guarantee
//! still holds for same-thread nesting (the outer frame on the same call
//! stack drains before it returns, as before) but is *not* guaranteed for two
//! genuinely concurrent callers on different threads — a non-drainer call may
//! return slightly before the drainer has processed its contribution. No
//! update is ever lost or reordered across the height boundary because of
//! this (the drainer will still process it before giving up); only the exact
//! instant "propagation from this specific call is externally visible"
//! becomes slightly fuzzy under real cross-thread contention, which no code
//! today depends on (there was no cross-thread coordination to depend on
//! before this).
//!
//! # Wave-parallel draining, cheaply
//!
//! Two cells at the *same* height can never depend on each other (height is
//! `1 + max(dep.height)`, so a dependency would force a strictly greater
//! height), which makes same-height ops mathematically safe to run
//! concurrently — the same principle spreadsheet engines like Excel's
//! multithreaded recalculation use: build the dependency chain once, cheaply,
//! then dispatch each *known-independent* level of it in bulk.
//!
//! An earlier version of this module took that further than it needed to: it
//! sharded the *queue itself* by height (a `DashMap`-backed bucket per
//! height, with its own lock, plus bookkeeping to move a coalescing cell's
//! entry between buckets if its height changed mid-tick) so that pushes at
//! different heights wouldn't contend on one lock. Measured against both a
//! plain single-cell/diamond microbenchmark and a dedicated multi-thread
//! contention benchmark, it was slower in *every* case — the fixed per-push
//! cost of that sharding exceeded whatever contention it avoided, even under
//! genuine concurrent load (a trivial 4-cell diamond went from ~650ns to
//! ~8µs; 8-thread contention throughput dropped from ~2M elem/s to ~1M elem/s).
//! The mistake was sharding the cheap, frequent *enqueue* path to serve the
//! rarer, bulkier *drain* path.
//!
//! This version keeps [`enqueue`]'s single-lock `BTreeMap` push exactly as
//! cheap as the non-parallel design, and moves all the "can this run in
//! parallel" work to the drain side, where it only has to happen once per
//! height per tick instead of once per push: [`SharedTick::pop_min_height_groups`]
//! pulls every op at the current minimum height out in one locked pass, grouped
//! by cell, and [`run_wave`] runs those groups — sequentially if there are few
//! (the common/resting case: a join has 2 inputs, most fan-out is a handful of
//! subscribers, and a pool dispatch's overhead alone would dwarf that), or
//! across the scheduler's own dedicated [`WAVE_POOL`] if the wave crosses
//! [`WAVE_THRESHOLD`] (genuinely wide fan-out — many independent cells settling
//! at once). That threshold defaults high and the pool is built lazily and
//! sized small on purpose: real graphs are overwhelmingly deep rather than
//! wide, and dispatching resting `.batched()` timer waves through a big shared
//! pool was measured burning most of a process's CPU at idle for no gain (see
//! [`DEFAULT_WAVE_THRESHOLD`]).
//!
//! Sharding by connected component (so genuinely unrelated graphs never share
//! a lock at all, instead of sharding by a graph-agnostic height number that
//! unrelated graphs still collide on) would help the "many unrelated threads"
//! case further, but needs its own careful design — dynamic topology changes
//! would require a concurrent union-find to merge domains safely — and
//! wasn't pursued here; this hybrid captures the parallelism win for the
//! common case (one graph, a wide wave) at effectively no cost to the small
//! case.
//!
//! `join`/`join_vec`'s implementations were also hardened for this: both used
//! to have each input side independently peek at a sibling's `.get()` to
//! build a combined value, which is safe only if the sides can never run at
//! the literal same instant — true before wave-parallel draining existed, not
//! true once a wave can genuinely run two same-height ops concurrently. Both
//! now hold a lock across their entire update-and-notify sequence so
//! whichever side's push lands last in the coalescing slot is guaranteed to
//! reflect both sides' latest values, not a stale peek at one that hadn't
//! updated yet.
//!
//! # Scope (Phase 0)
//!
//! Coalescing is last-write-wins, which is correct for **behavior** cells
//! (`map`/`filter`/`join`/`switch_map` — latest-value semantics, exactly where
//! the glitch lives). It is **not** correct for **event** operators
//! (`scan`/`pairwise`/`buffer`/`zip`/`merge`), where dropping an intermediate
//! value changes the result; scoping the opt-in away from those is the next
//! phase. Height is memoized per tick (topology is assumed stable within a
//! tick); recompute-at-pop for switch_map rewiring mid-drain is a later phase
//! too.
//!
//! Because a batch defers value settlement to the drain, a cell read *inside*
//! the batch (before the closing brace) still sees its pre-batch value — the
//! glitch-free trade-off, and the reason this is opt-in.

use std::{
    collections::BTreeMap,
    sync::{
        LazyLock,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

use parking_lot::Mutex;
use rustc_hash::{FxHashMap, FxHashSet};
use uuid::Uuid;

use crate::traits::DepNode;

thread_local! {
    /// Reentrancy depth of the active [`no_coalesce`] construction scope on
    /// THIS thread. Construction-time only, so it stays thread-local: the
    /// resulting stamp is baked onto each cell at birth (see
    /// [`birth_no_coalesce`]) and rides the cell for its lifetime — no
    /// per-tick, cross-thread coordination is needed for it, unlike the tick
    /// queue below.
    static NO_COALESCE_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    /// THIS thread's [`batch`] nesting depth. Only a thread's *outermost*
    /// batch (the close that brings this back to 0) is eligible to drain, so
    /// same-thread nested batches never drain mid-`f()` — preserving "all of a
    /// batch's sets are enqueued before any of it drains." Thread-local
    /// because it tracks one call stack's nesting; the cross-thread batch
    /// count is [`SharedTick::depth`].
    static BATCH_NEST: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

// Height-cache invalidation is **per-node**, not global. Each cell carries its
// own `height_epoch` (see `CellInner::height_epoch`); an edge change bumps only
// the changed cell and its transitive dependents' epochs
// (`crate::cell::invalidate_height_cone`), so a topology-churning knit no longer
// flushes every cached height in the process. There is deliberately no global
// epoch counter here anymore — a cached height is validated against its own
// node's epoch in `height_dfs`.

/// The process-wide propagation tick queue: height-ordered and
/// last-write-wins-coalesced per cell (see module docs). Every thread's
/// deferred ops share this one structure — the `Mutex` is what makes
/// [`batch`]'s glitch-free guarantee a cross-thread one instead of a
/// per-thread one that silently doesn't hold at the seams where two threads'
/// timers/dispatch converge on a shared cell.
struct SharedTick {
    /// Height-ordered frontier: `(height, id, seq) -> deferred op`. `BTreeMap`
    /// pops the minimum key first, giving height ordering; the `seq`
    /// tiebreaker keeps a `no_coalesce` cell's multiple ops distinct **and**
    /// in arrival order (a cell's keys share `(height, id)`, so they sort
    /// contiguously by `seq`). Coalescing cells keep exactly one live key at
    /// a time.
    order: BTreeMap<(u64, Uuid, u64), Box<dyn FnOnce() + Send>>,
    /// For coalescing cells only: `id -> (height, seq)` of its single live
    /// entry in `order`, so a re-notify can find and drop the superseded op
    /// (last-write-wins) before re-inserting. `no_coalesce` cells are never
    /// tracked here — each of their ops survives to the drain.
    scheduled: FxHashMap<Uuid, (u64, u64)>,
    /// Monotonic sequence stamp, ordering ops enqueued at the same
    /// `(height, id)` by arrival, across every thread.
    seq: u64,
    /// Number of currently-open [`batch`] calls, summed across every thread.
    /// Together with `draining` it gates whether a fresh `notify` defers:
    /// deferral is on whenever `depth > 0 || draining` (a batch is open
    /// anywhere, or a drain is in flight and a popped op's fanout must enqueue
    /// into the current drain rather than cascade eagerly).
    depth: u32,
    /// Whether a drain is currently in progress. At most one thread drains at
    /// a time; this flag is the mutual exclusion. A thread's outermost
    /// [`batch`] close claims the drain (sets this true) only if it's false
    /// and there's pending work; the drainer clears it the instant the queue
    /// runs empty. No thread ever *parks* as a persistent drainer — draining
    /// happens in short bursts at batch-close boundaries and is handed off via
    /// this flag, so a thread that keeps opening batches can never starve the
    /// drainer (there is no "wait for the queue to quiesce" loop to starve).
    draining: bool,
}

impl SharedTick {
    /// Enqueue `run` for cell `id` at `height`. When `coalesce` is true and the
    /// cell is already queued, its previous op is dropped (last-write-wins).
    /// When false (a `no_coalesce` cell), every op is kept as a distinct
    /// arrival-ordered key so event semantics survive.
    fn enqueue_locked(
        &mut self,
        id: Uuid,
        height: u64,
        coalesce: bool,
        run: Box<dyn FnOnce() + Send>,
    ) {
        let seq = self.seq;
        self.seq += 1;
        if coalesce && let Some((prev_height, prev_seq)) = self.scheduled.insert(id, (height, seq))
        {
            // Drop the superseded op (and the value/cell it captured). We hold
            // no cell lock here, so a cascading Arc drop is safe. (The insert
            // short-circuits away for `no_coalesce` cells — they're never
            // tracked in `scheduled`.)
            self.order.remove(&(prev_height, id, prev_seq));
        }
        self.order.insert((height, id, seq), run);
    }

    /// Remove every op at the current minimum height, **grouped by cell id**,
    /// each group in `seq` (arrival) order; or `None` if the frontier is empty.
    ///
    /// The parallel unit is a *cell*, not an op. Distinct groups are distinct
    /// cells at the same height — mathematically independent (see [`run_wave`]'s
    /// docs), safe to run concurrently. Ops **within** a group all target the
    /// same cell: a `no_coalesce` cell can have several ops queued here at once,
    /// and running its value-settle + fanout concurrently with itself would both
    /// reorder its events and invoke its subscribers on two threads at once. So
    /// a group must run sequentially in arrival order. Coalescing cells always
    /// have exactly one op, hence a one-element group.
    ///
    /// `order` is a `BTreeMap` keyed `(height, id, seq)`, so a cell's ops sort
    /// contiguously by `seq` within the height — one linear pass groups them
    /// with no map or sort, and the `scheduled` back-pointer cleanup (formerly
    /// in `pop_min`) is folded inline.
    fn pop_min_height_groups(&mut self) -> Option<Vec<Vec<Box<dyn FnOnce() + Send>>>> {
        let target = self.order.keys().next().map(|&(h, _, _)| h)?;
        let mut groups: Vec<Vec<Box<dyn FnOnce() + Send>>> = Vec::new();
        let mut cur_id: Option<Uuid> = None;
        while matches!(self.order.keys().next(), Some(&(h, _, _)) if h == target) {
            let ((_, id, seq), run) = self.order.pop_first().expect("just peeked a matching key");
            // Clear the coalescing back-pointer only if it still names the op we
            // popped: a coalescing cell has exactly one entry (this one); a
            // `no_coalesce` cell has none; a cell re-coalesced at a new seq keeps
            // its newer entry.
            if let Some(&(_, live_seq)) = self.scheduled.get(&id)
                && live_seq == seq
            {
                self.scheduled.remove(&id);
            }
            // Same id as the previous pop → same cell → extend its group (keys
            // are already seq-ordered). Otherwise start a new group.
            if cur_id == Some(id) {
                groups
                    .last_mut()
                    .expect("cur_id set implies a group exists")
                    .push(run);
            } else {
                cur_id = Some(id);
                groups.push(vec![run]);
            }
        }
        Some(groups)
    }
}

static TICK: LazyLock<Mutex<SharedTick>> = LazyLock::new(|| {
    Mutex::new(SharedTick {
        order: BTreeMap::new(),
        scheduled: FxHashMap::default(),
        seq: 0,
        depth: 0,
        draining: false,
    })
});

/// Fast, lock-free "should a fresh notify defer" flag, kept in sync with
/// `depth > 0 || draining` under the same critical sections that mutate
/// either. `notify`'s hot path ([`tick_active`]) reads this instead of
/// locking `TICK`, so the common "no batch open, no drain running" case pays
/// one relaxed atomic load instead of a mutex acquisition. It's a hint, not
/// the source of truth: [`enqueue`] re-validates under the lock before
/// deferring, so a stale `true` read (racing a batch/drain that just ended)
/// safely falls back to running synchronously instead of enqueueing into a
/// queue nobody will drain.
static TICK_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Set `TICK_ACTIVE` from the authoritative state under the lock. Call after
/// any mutation of `depth` or `draining`.
fn refresh_tick_active(t: &SharedTick) {
    TICK_ACTIVE.store(t.depth > 0 || t.draining, Ordering::Relaxed);
}

/// The cell's propagation height, `1 + max(dep.height)` (sources are 0), read
/// from its persistent per-node cache when the cache is current for `epoch` and
/// recomputed (and re-cached) otherwise. In a stable topology this is one atomic
/// load; `deps()` is walked only on the first read after an edge change.
fn compute_height(node: &dyn DepNode) -> u64 {
    let mut stack = FxHashSet::default();
    height_dfs(node, &mut stack)
}

/// Height DFS backing [`compute_height`]. Each node's [`DepNode::height_cache`]
/// packs `(height_epoch << 32) | height` and is validated against that node's
/// *own* [`DepNode::height_epoch`] — so a settled subgraph stays cached even
/// while an unrelated (or downstream) part of the graph is churning, and only
/// nodes whose cone was invalidated recompute. `stack` breaks dependency cycles
/// — a back-edge to a node already on the current path contributes height 0
/// rather than recursing forever. Nodes without an epoch (non-cell `DepNode`s)
/// have no stable cache slot and are recomputed each read.
fn height_dfs(node: &dyn DepNode, stack: &mut FxHashSet<Uuid>) -> u64 {
    let epoch = node
        .height_epoch()
        .map(|e| e.load(Ordering::Relaxed) as u32);
    if let (Some(cache), Some(epoch)) = (node.height_cache(), epoch) {
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
        height = height.max(height_dfs(dep.as_ref(), stack) + 1);
    }
    stack.remove(&id);
    if let (Some(cache), Some(epoch)) = (node.height_cache(), epoch) {
        cache.store(((epoch as u64) << 32) | height, Ordering::Relaxed);
    }
    height
}

/// Whether a batch is active on any thread. Cheap — one relaxed atomic load.
/// This is the single check `notify` pays on the synchronous hot path when
/// the `scheduler` feature is on but no batch is open.
pub(crate) fn tick_active() -> bool {
    TICK_ACTIVE.load(Ordering::Relaxed)
}

/// Defer `run` for cell `id` (height computed from `node`) if a batch is open
/// anywhere, re-validated under the lock; otherwise run it immediately,
/// synchronously, on the calling thread. Called by `notify` as a same-lock
/// combined check-and-act — splitting the check and the enqueue across two
/// lock acquisitions would reopen a stranding race (a batch closing in the
/// gap between them).
///
/// `terminal` is true for a `Complete`/`Error` notify. A terminal op is never
/// coalesced: an operator that emits a value **and then completes** in one
/// callback (e.g. `take`'s last element, `last`) enqueues `Value` then
/// `Complete` for the same cell in one batch, and if the terminal coalesced it
/// would last-write-wins-drop the value it follows — the final value would
/// silently vanish under `batch` (confirmed by repro). Kept non-coalescing, the
/// terminal takes a distinct later `seq`, so the value drains first and the
/// terminal after it. (A value can't legitimately follow a terminal on the same
/// cell — `notify` early-returns once the cell is completed/errored — so this
/// never leaves a terminal ordered before a live value.)
pub(crate) fn enqueue(id: Uuid, node: &dyn DepNode, terminal: bool, run: Box<dyn FnOnce() + Send>) {
    let mut guard = TICK.lock();
    // Defer only if a batch is open somewhere, or a drain is in flight (a
    // popped op's fanout must land in the current drain's queue, not cascade
    // eagerly). Otherwise run synchronously on this thread — the default
    // eager path. Re-validated here under the lock, so a stale `TICK_ACTIVE`
    // hint can't strand an op in a queue nobody will drain.
    if guard.depth == 0 && !guard.draining {
        drop(guard);
        run();
        return;
    }
    let height = compute_height(node);
    let coalesce = !node.no_coalesce() && !terminal;
    guard.enqueue_locked(id, height, coalesce, run);
}

/// Default minimum number of distinct-cell *groups* in one height-wave before
/// dispatching it across the wave pool is worth the cost. Overridable at
/// startup via `HYPHAE_WAVE_THRESHOLD`.
///
/// This is deliberately high. An earlier version used `8`, which made resting
/// `Source::batched()` timer ticks — which produce wide same-height waves
/// continuously — dispatch through rayon's *global* pool nonstop, burning >50%
/// of cycles in crossbeam-epoch pinning + work-steal loops (~750% CPU at idle
/// on a 24-core box) to parallelize waves whose per-group work is a trivial
/// `Cell::fanout`. Real reactive graphs are overwhelmingly deep, not wide: even
/// a heavy 1s+ rebuild wave was measured using only 1–2 cores, so parallel
/// dispatch buys little and its fixed per-wave cost is pure loss at rest. A high
/// threshold keeps the common/resting case sequential — and because
/// [`WAVE_POOL`] is built lazily, a process whose waves never cross it spawns
/// zero wave threads and pays zero idle cost.
const DEFAULT_WAVE_THRESHOLD: usize = 64;

/// Default wave-pool size cap. Kept small because the workload is deep, not
/// wide; overridable via `HYPHAE_WAVE_THREADS` (`0` disables the parallel path
/// entirely — every wave runs sequentially).
const DEFAULT_WAVE_THREADS_CAP: usize = 4;

fn env_usize(key: &str) -> Option<usize> {
    std::env::var(key).ok().and_then(|v| v.trim().parse().ok())
}

/// Group threshold, seeded once from `HYPHAE_WAVE_THRESHOLD` or the default.
/// Atomic (not a plain `usize`) only so tests can force the parallel path at a
/// small width via [`set_wave_threshold_for_test`]; the steady-state read is a
/// single relaxed load.
static WAVE_THRESHOLD: LazyLock<AtomicUsize> = LazyLock::new(|| {
    AtomicUsize::new(env_usize("HYPHAE_WAVE_THRESHOLD").unwrap_or(DEFAULT_WAVE_THRESHOLD))
});

fn wave_threshold() -> usize {
    WAVE_THRESHOLD.load(Ordering::Relaxed)
}

/// Test-only knob: override the wave-parallelism group threshold at runtime so
/// the parallelism/torn-value tests can exercise the real parallel drain path at
/// a small width instead of the high production default. Not part of the stable
/// API — do not rely on it outside tests.
#[doc(hidden)]
pub fn set_wave_threshold_for_test(groups: usize) {
    WAVE_THRESHOLD.store(groups, Ordering::Relaxed);
}

/// Configured wave-pool thread count, resolved once. `0` means "no
/// parallelism" — [`run_wave`] then always runs sequentially and [`WAVE_POOL`]
/// is never built.
static WAVE_THREADS: LazyLock<usize> = LazyLock::new(|| {
    env_usize("HYPHAE_WAVE_THREADS").unwrap_or_else(|| {
        std::thread::available_parallelism()
            .map(|n| n.get().min(DEFAULT_WAVE_THREADS_CAP))
            .unwrap_or(1)
    })
});

/// The scheduler's **dedicated**, named wave-parallelism pool — `None` when
/// disabled (`HYPHAE_WAVE_THREADS=0`). Built lazily on the first wave that
/// genuinely crosses [`WAVE_THRESHOLD`], so a process that never has a wide wave
/// never constructs it (and never spawns its threads).
///
/// Dedicated rather than rayon's global pool for two reasons the field data
/// surfaced: (1) sizing is decoupled from the ambient core count, so a 24-core
/// host doesn't wake 24 workers for a wave that needs 2; (2) the workers are
/// *named* (`hyphae-wave-N`) — global-pool workers inherit the name of whichever
/// thread first touches the pool (they showed up as `hyphae-timer-re`), which
/// poisons every profile taken of a process that uses hyphae timers.
static WAVE_POOL: LazyLock<Option<rayon::ThreadPool>> = LazyLock::new(|| {
    let threads = *WAVE_THREADS;
    if threads == 0 {
        return None;
    }
    rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .thread_name(|i| format!("hyphae-wave-{i}"))
        .build()
        .ok()
});

/// Run one height's worth of ops, grouped by cell (see
/// [`SharedTick::pop_min_height_groups`]). Distinct groups are distinct cells at
/// the same height: height is `1 + max(dep.height)`, so if one group's cell
/// depended on another's it would have a strictly greater height and never land
/// in this wave — each group only writes its own cell's value and fans out to
/// strictly-higher-height subscribers, which enqueue into a later wave. So
/// distinct groups are safe to run concurrently. Within a group every op
/// targets the *same* cell and runs sequentially in arrival order — a
/// `no_coalesce` cell can queue several ops in one wave, and running its fanout
/// concurrently with itself would reorder its events and race its subscribers.
///
/// The threshold compares the number of *groups* (distinct cells), not raw ops:
/// a single event cell fired 32× is one group and stays sequential (as it must
/// to preserve order), while a genuinely wide wave that crosses
/// [`WAVE_THRESHOLD`] gets real parallelism on the dedicated [`WAVE_POOL`].
/// Panics are caught per-op so one buggy callback can't strand or corrupt
/// unrelated work; every payload seen is returned for the caller to pick one to
/// re-raise.
#[cfg(not(target_arch = "wasm32"))]
fn run_wave(groups: Vec<Vec<Box<dyn FnOnce() + Send>>>) -> Vec<Box<dyn std::any::Any + Send>> {
    fn run_group(group: Vec<Box<dyn FnOnce() + Send>>) -> Vec<Box<dyn std::any::Any + Send>> {
        group
            .into_iter()
            .filter_map(|run| std::panic::catch_unwind(std::panic::AssertUnwindSafe(run)).err())
            .collect()
    }
    fn run_sequential(
        groups: Vec<Vec<Box<dyn FnOnce() + Send>>>,
    ) -> Vec<Box<dyn std::any::Any + Send>> {
        groups.into_iter().flat_map(run_group).collect()
    }
    // Common/resting case: below the threshold, or parallelism disabled — run
    // sequentially and never touch the pool (so it stays unbuilt).
    if groups.len() < wave_threshold() {
        return run_sequential(groups);
    }
    match WAVE_POOL.as_ref() {
        Some(pool) => {
            use rayon::prelude::*;
            pool.install(|| groups.into_par_iter().flat_map_iter(run_group).collect())
        }
        None => run_sequential(groups),
    }
}

/// wasm is single-threaded — no real parallelism to exploit, and no
/// cross-thread race to guard against either, but this module still needs to
/// compile and behave correctly there (the `scheduler` feature isn't
/// native-only). Groups run in order, ops within each in arrival order.
#[cfg(target_arch = "wasm32")]
fn run_wave(groups: Vec<Vec<Box<dyn FnOnce() + Send>>>) -> Vec<Box<dyn std::any::Any + Send>> {
    groups
        .into_iter()
        .flatten()
        .filter_map(|run| std::panic::catch_unwind(std::panic::AssertUnwindSafe(run)).err())
        .collect()
}

/// Drain the shared queue one height-wave at a time until it is empty, then
/// release the drain (`draining = false`) and return. Called only by the
/// thread that claimed the drain at its outermost [`batch`] close (see
/// [`batch`]).
///
/// Unlike a persistent drainer, this never waits for other threads' batches
/// to quiesce — there is no park and no "wait for the count to settle" loop,
/// so nothing to starve. It drains whatever is queued right now and stops the
/// instant the queue is empty. Any op enqueued afterward (by a still-open
/// batch's `f()`, or a batch that opens later) is drained by whichever
/// outermost batch-close next observes pending work with no drain in flight:
/// draining hands off at close boundaries rather than one unlucky thread
/// being pinned as the drainer while others keep it from ever terminating.
///
/// Completeness (no op is stranded) holds because the empty-check and the
/// `draining = false` release happen together under the lock: an op enqueued
/// before that release is popped by this loop; an op enqueued after it is
/// seen by the next outermost close (which finds `!draining` and a non-empty
/// queue, and drains). While this runs, `draining` is true, so a popped op's
/// fanout `enqueue`s into this same queue (see [`enqueue`]'s gate) instead of
/// cascading eagerly.
///
/// The lock is dropped around every `run_wave` — a popped op's fanout (or a
/// nested `batch`) re-enters `enqueue`/`batch` and needs the lock. Per-op
/// panics are caught in [`run_wave`] so one buggy callback can't strand other
/// concurrent batches' queued work; the first panic seen is returned for the
/// caller to re-raise after the drain has fully completed.
fn drain() -> Option<Box<dyn std::any::Any + Send>> {
    let mut first_panic: Option<Box<dyn std::any::Any + Send>> = None;
    let mut waves = 0usize;
    loop {
        let groups = {
            let mut guard = TICK.lock();
            // Bounded rotation: after a stint's worth of waves, hand the drain
            // off *if another batch is still open* (`depth > 0`, so a close is
            // coming to reclaim it). This caps any single `batch()` call's
            // drain time, so a burst of concurrent batchers rotate the drain
            // among themselves instead of pinning the first claimant as the
            // sole drainer for the whole storm. The `depth > 0` check and the
            // release happen under this one lock, so it's strand-free: if the
            // last other batch just closed (`depth == 0`), we don't hand off —
            // there'd be no one to reclaim — and drain to completion instead.
            if waves >= DRAIN_STINT_WAVES && guard.depth > 0 {
                guard.draining = false;
                refresh_tick_active(&guard);
                return first_panic;
            }
            match guard.pop_min_height_groups() {
                Some(groups) => groups,
                None => {
                    guard.draining = false;
                    refresh_tick_active(&guard);
                    return first_panic;
                }
            }
        };
        let panics = run_wave(groups);
        if first_panic.is_none() {
            first_panic = panics.into_iter().next();
        }
        waves += 1;
    }
}

/// How many height-waves one thread drains before handing the drain off to
/// another open batch (see [`drain`]). Large enough that the common
/// single-threaded case (which drains a whole tick in far fewer waves and then
/// finds the queue empty) never hits it, small enough that under a many-thread
/// batch storm no single `batch()` call is pinned draining for long.
const DRAIN_STINT_WAVES: usize = 64;

/// Construct cells that opt out of the scheduler's last-write-wins coalescing.
///
/// Every cell created while this scope is on the stack — via [`Cell::new`], and
/// so via operator `materialize` too — is stamped
/// [`Cell::no_coalesce`](crate::Cell::no_coalesce) at birth. Use it to exempt an
/// *event-semantic subgraph* as a unit: the stateful operator (a `scan`,
/// `pairwise`, or a hand-rolled edge-detector `map`) **and the sources feeding
/// it**, because coalescing a source upstream of the operator would drop the
/// intermediates before they ever reach it — tagging only the operator's own
/// cell would still starve it.
///
/// The stamp rides each cell for its lifetime, so it holds when the cell later
/// fires under [`batch`], regardless of where the batch is opened. Nestable;
/// composes with `batch` (you may open a batch inside or outside this scope).
///
/// Only cells *born inside* the closure are affected — a factory registered
/// here but invoked later builds its cells outside the scope and is **not**
/// stamped. Wrap the actual construction, not a deferred builder.
pub fn no_coalesce<R>(f: impl FnOnce() -> R) -> R {
    NO_COALESCE_DEPTH.with(|d| d.set(d.get() + 1));
    struct Guard;
    impl Drop for Guard {
        fn drop(&mut self) {
            NO_COALESCE_DEPTH.with(|d| d.set(d.get() - 1));
        }
    }
    let _guard = Guard;
    f()
}

/// Whether a [`no_coalesce`] construction scope is active on this thread. Read
/// by `Cell::new`/`with_metrics` to stamp a cell's coalescing policy at birth.
pub(crate) fn birth_no_coalesce() -> bool {
    NO_COALESCE_DEPTH.with(|d| d.get() > 0)
}

/// Run `f` as a single propagation batch.
///
/// Every `set`/`notify` on ANY thread while `f` runs enqueues into the one
/// shared tick instead of cascading. Draining is deferred until `f` (and any
/// batch nested inside it, on this thread or another) has fully returned —
/// drain only runs at the closing brace, never interleaved with `f`'s own
/// execution. Draining any earlier would let a diamond join fire on a partial
/// set of inputs, since the second input might not have arrived yet.
///
/// Nested `batch` calls — genuinely nested on the same thread's call stack,
/// or a different thread racing this one — join the same shared tick rather
/// than opening a second one. Draining is claimed at a thread's *outermost*
/// batch close (its [`BATCH_NEST`] returning to 0), by whichever such close
/// finds no drain already in flight and pending work; that drain runs the
/// queue empty in short bursts and hands off (see [`drain`]). A same-thread
/// outermost call still observes full settlement by the time it returns
/// (single-threaded, it always claims and fully drains its own work). A
/// genuinely concurrent thread's close may return slightly before another
/// thread's active drain has processed its contribution — no update is lost
/// or reordered by this, only the instant "settled" becomes externally
/// visible is fuzzy under real cross-thread contention that had no
/// coordination at all before this module existed.
///
/// A panic inside `f` (or a nested `batch` inside it) is caught here; the
/// close-and-drain still runs to completion — a panic must not strand other
/// threads' unrelated work already queued alongside this call's — and then
/// `f`'s panic is resumed to its caller.
///
/// Outside the `scheduler` feature this is a behavior-preserving passthrough;
/// see the module docs for the deferred semantics when the feature is on.
pub fn batch<R>(f: impl FnOnce() -> R) -> R {
    let outermost = BATCH_NEST.with(|n| {
        let v = n.get() + 1;
        n.set(v);
        v == 1
    });
    {
        let mut guard = TICK.lock();
        guard.depth += 1;
        refresh_tick_active(&guard);
    }

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));

    // Close this batch and decide whether to drain. Claim the drain only from
    // a thread's OUTERMOST batch (nested same-thread batches must not drain
    // mid-`f()`), only if no drain is already in flight, and only if there is
    // pending work — the empty-check and the `draining = true` claim happen
    // together under the lock, which is what makes the hand-off strand-free.
    let claimed_drain = {
        let mut guard = TICK.lock();
        guard.depth -= 1;
        let claim = outermost && !guard.draining && !guard.order.is_empty();
        if claim {
            guard.draining = true;
        }
        refresh_tick_active(&guard);
        claim
    };
    BATCH_NEST.with(|n| n.set(n.get() - 1));

    let drain_panic = if claimed_drain { drain() } else { None };

    match result {
        Ok(value) => {
            if let Some(payload) = drain_panic {
                std::panic::resume_unwind(payload);
            }
            value
        }
        // `f`'s own panic is the caller-relevant one and takes precedence.
        Err(payload) => std::panic::resume_unwind(payload),
    }
}
