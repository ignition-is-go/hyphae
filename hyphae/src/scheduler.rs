//! Propagation scheduler — deferred, ordered, glitch-free flushes under an
//! explicit [`batch`] boundary.
//!
//! Outside a [`batch`] (and outside a drain), propagation takes hyphae's
//! synchronous eager-push path on the writing thread.
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
//! or a drain in flight). A write a thread makes inside its own open batch is
//! **staged** on that thread and reaches the queue only when the thread's
//! outermost `batch()` closes, in one step under the lock. A batch is
//! therefore atomic to observers: a drain, on any thread, runs all of a
//! batch's writes or none of them, never the first half while the batch is
//! still open. A write from a thread that holds no batch, made while another
//! thread's batch is open, goes straight to the queue as before. Draining is
//! **claimed at a thread's outermost `batch()` close** by whichever such
//! close finds `draining` false and the queue non-empty; it sets `draining` (the single-drainer mutex) and drains the
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
//! A `CellMap` or `CellSet` stores its rows when a write is made, before the
//! batch that made the write commits. Two batches that store rows for the
//! same key commit as one group, in one step, when the later of them closes,
//! so the diffs of a key reach the queue in the order its rows were stored.
//! A batch that closes first leaves its writes with the group and returns.
//! It never waits. See [`order`].
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
//! # Scheduling domains
//!
//! "Process-wide" above describes the **root domain**, which holds every cell
//! of a program that never creates a child [`Domain`]. Each child domain has a
//! tick of its own with the same rules, so one child's batches and drains
//! never wait on root's or a sibling's. Everything in this module works on
//! one domain's tick: `notify` picks it from the cell, [`batch`] from the
//! calling thread's ambient domain. A thread running one domain's
//! propagation writes only that domain. See [`domain`] for the boundary
//! between root and its children.
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
//! Each ordinary pending cell stores its operation inline in one ordered frontier
//! record. Only cells with several arrival-ordered event/terminal operations
//! use reusable arena storage. Repeated coalescing writes replace the inline
//! operation directly. Parallel dispatch stays on the drain side, once per height
//! rather than once per push: [`SharedTick::pop_min_height_into`]
//! pulls every op at the current minimum height out in one locked pass, grouped
//! by cell, and [`run_wave`] runs those groups — sequentially if there are few
//! (the common/resting case: a join has 2 inputs, most fan-out is a handful of
//! subscribers, and a pool dispatch's overhead alone would dwarf that), or
//! across Hyphae's shared dedicated worker pool if the wave crosses
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
//! wasn't pursued here; dispatch still depends on the width of the current
//! height wave.
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
//! tick); recompute-at-pop for `switch_map` rewiring mid-drain is a later phase
//! too.
//!
//! Because a batch defers value settlement to the drain, a cell read *inside*
//! the batch (before the closing brace) still sees its pre-batch value — the
//! glitch-free trade-off, and the reason this is opt-in.

use std::{
    cell::RefCell,
    sync::{
        Arc, LazyLock,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

use parking_lot::{Condvar, Mutex};
use rustc_hash::FxHashMap;
use uuid::Uuid;

use crate::traits::DepNode;

pub mod domain;
mod frontier;
pub(crate) mod order;
mod ready;

use domain::DomainInner;
pub use domain::{Domain, DomainError};
use order::{Claim, Groups, Writer};
use ready::{PendingRun, ReadyQueue, Wave};

thread_local! {
    /// Reentrancy depth of the active [`no_coalesce`] construction scope on
    /// THIS thread. Construction-time only, so it stays thread-local: the
    /// resulting stamp is baked onto each cell at birth (see
    /// [`birth_no_coalesce`]) and rides the cell for its lifetime — no
    /// per-tick, cross-thread coordination is needed for it, unlike the tick
    /// queue below.
    static NO_COALESCE_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    /// THIS thread's open batches, one record per domain. A write this
    /// thread makes to a cell of a domain it holds a batch on is staged in
    /// the record and reaches the shared tick only when the thread's
    /// outermost batch of that domain closes, all at once. Until then no
    /// drain, on any thread, can run part of the batch.
    static OPEN_BATCHES: RefCell<Vec<OpenBatch>> = const { RefCell::new(Vec::new()) };
    /// Length of [`OPEN_BATCHES`], readable without borrowing it. A thread
    /// that holds no batch pays one load to learn it has nothing to stage.
    static OPEN_BATCH_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// The staging list of this thread's last committed batch, kept so the
    /// next batch does not allocate one.
    static SPARE_STAGING: std::cell::Cell<Vec<Staged>> = const { std::cell::Cell::new(Vec::new()) };
    /// The claim list of this thread's last committed batch, kept likewise.
    static SPARE_CLAIMS: std::cell::Cell<Vec<Claim>> = const { std::cell::Cell::new(Vec::new()) };
}

/// Largest staging list kept for reuse. A batch that staged more gives its
/// memory back.
const SPARE_STAGING_LIMIT: usize = 256;

/// One write staged by an open batch.
struct Staged {
    id: Uuid,
    height: u64,
    coalesce: bool,
    /// The batch's [`OpenBatch::stamp`] when this was last written. Orders
    /// the writes of batches that commit as one group.
    stamp: u64,
    run: PendingRun,
}

/// One thread's open batch on one domain.
struct OpenBatch {
    domain: u64,
    /// Same-thread nesting. Only the close that brings this to zero commits.
    nest: u32,
    staged: Vec<Staged>,
    /// Index in `staged` of each cell's latest write and whether it coalesces.
    /// Empty until `staged` outgrows [`STAGED_SCAN`].
    latest: FxHashMap<Uuid, (usize, bool)>,
    /// Stamp of the last row this batch stored under a
    /// [`WriteOrder`](order::WriteOrder), 0 before the first. A stamp is
    /// later than that of every row stored before it in the same store and
    /// of every earlier one of this batch.
    stamp: u64,
    /// This batch as a pending writer. `None` until it stores such a row.
    writer: Option<Arc<Writer>>,
    /// The stores it holds keys pending on, released when its writes are in
    /// the tick.
    claims: Vec<Claim>,
}

/// What a thread's outermost batch held when it closed.
struct ClosedBatch {
    staged: Vec<Staged>,
    writer: Option<Arc<Writer>>,
    claims: Vec<Claim>,
}

/// Longest staging list searched linearly for a cell's latest write. Most
/// batches write a handful of cells and never build the index.
const STAGED_SCAN: usize = 8;

impl OpenBatch {
    /// Stage `op`. A coalescing write whose cell's latest staged write also
    /// coalesces takes that write's place, exactly as the tick would coalesce
    /// the two, and the superseded write is returned for the caller to drop.
    #[inline]
    fn stage(&mut self, mut op: Staged) -> Option<PendingRun> {
        op.stamp = self.stamp;
        let indexed = self.staged.len() > STAGED_SCAN;
        if indexed && self.latest.is_empty() {
            for (index, staged) in self.staged.iter().enumerate() {
                self.latest.insert(staged.id, (index, staged.coalesce));
            }
        }
        let prior = if indexed {
            self.latest.get(&op.id).copied()
        } else {
            self.staged
                .iter()
                .rposition(|staged| staged.id == op.id)
                .and_then(|index| Some((index, self.staged.get(index)?.coalesce)))
        };
        if let Some((index, true)) = prior
            && op.coalesce
            && let Some(slot) = self.staged.get_mut(index)
        {
            slot.height = op.height;
            // The cell now holds this write, so it follows whatever another
            // batch of the same group wrote before it.
            slot.stamp = op.stamp;
            return Some(std::mem::replace(&mut slot.run, op.run));
        }
        if indexed {
            self.latest.insert(op.id, (self.staged.len(), op.coalesce));
        }
        self.staged.push(op);
        None
    }
}

/// Record one more open batch of `domain` on this thread.
fn open_batch(domain: u64) {
    OPEN_BATCHES.with(|open| {
        let mut open = open.borrow_mut();
        if let Some(batch) = open.iter_mut().find(|batch| batch.domain == domain) {
            batch.nest = batch.nest.saturating_add(1);
        } else {
            open.push(OpenBatch {
                domain,
                nest: 1,
                staged: SPARE_STAGING.take(),
                latest: FxHashMap::default(),
                stamp: 0,
                writer: None,
                claims: SPARE_CLAIMS.take(),
            });
            OPEN_BATCH_COUNT.set(open.len());
        }
    });
}

/// Run `f` on this thread's open batch of `domain`, if it holds one.
fn with_open_batch<R>(domain: u64, f: impl FnOnce(&mut OpenBatch) -> R) -> Option<R> {
    if OPEN_BATCH_COUNT.get() == 0 {
        return None;
    }
    OPEN_BATCHES.with_borrow_mut(|open| open.iter_mut().find(|batch| batch.domain == domain).map(f))
}

/// Close one batch of `domain` on this thread. Returns what it held when it
/// was the thread's outermost batch of that domain.
fn close_batch(domain: u64) -> Option<ClosedBatch> {
    OPEN_BATCHES.with(|open| {
        let mut open = open.borrow_mut();
        let index = open.iter().position(|batch| batch.domain == domain)?;
        let batch = open.get_mut(index)?;
        batch.nest = batch.nest.saturating_sub(1);
        if batch.nest > 0 {
            return None;
        }
        let batch = open.swap_remove(index);
        OPEN_BATCH_COUNT.set(open.len());
        Some(ClosedBatch {
            staged: batch.staged,
            writer: batch.writer,
            claims: batch.claims,
        })
    })
}

/// Stage `run` in this thread's open batch of `domain`, or give it back when
/// the thread holds none.
fn stage(
    domain: u64,
    id: Uuid,
    node: &dyn DepNode,
    terminal: bool,
    run: PendingRun,
) -> Result<(), PendingRun> {
    if OPEN_BATCH_COUNT.get() == 0 {
        return Err(run);
    }
    // Walking the dependencies can drop a node, and a dropping node can write.
    // Neither may happen while the staging list is borrowed.
    let height = compute_height(node);
    let coalesce = !node.no_coalesce() && !terminal;
    let superseded = OPEN_BATCHES.with_borrow_mut(|open| {
        match open.iter_mut().find(|batch| batch.domain == domain) {
            Some(batch) => Ok(batch.stage(Staged {
                id,
                height,
                coalesce,
                stamp: 0,
                run,
            })),
            None => Err(run),
        }
    })?;
    // A superseded write can own the last handle of a cell, whose drop may
    // write again, so it is destroyed once the list is no longer borrowed.
    drop(superseded);
    Ok(())
}

/// Hides this thread's open batch of one domain while the thread drains that
/// domain's tick, so a write made by a drained op goes to the tick instead of
/// into the batch. Restores the batch when dropped.
struct SuspendedBatch(Option<OpenBatch>);

impl SuspendedBatch {
    fn of(domain: u64) -> Self {
        if OPEN_BATCH_COUNT.get() == 0 {
            return Self(None);
        }
        Self(OPEN_BATCHES.with(|open| {
            let mut open = open.borrow_mut();
            let index = open.iter().position(|batch| batch.domain == domain)?;
            let batch = open.swap_remove(index);
            OPEN_BATCH_COUNT.set(open.len());
            Some(batch)
        }))
    }
}

impl Drop for SuspendedBatch {
    fn drop(&mut self) {
        if let Some(batch) = self.0.take() {
            OPEN_BATCHES.with(|open| {
                let mut open = open.borrow_mut();
                open.push(batch);
                OPEN_BATCH_COUNT.set(open.len());
            });
        }
    }
}

/// One domain's propagation tick queue (the root domain's is the process-wide
/// [`TICK`], a child domain's lives in its [`DomainInner`]): height-ordered and
/// last-write-wins-coalesced per cell (see module docs). Every thread's
/// deferred ops share this one structure — the `Mutex` is what makes
/// [`batch`]'s glitch-free guarantee a cross-thread one instead of a
/// per-thread one that silently doesn't hold at the seams where two threads'
/// timers/dispatch converge on a shared cell.
struct SharedTick {
    ready: ReadyQueue,
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
    /// Batches that must commit together to keep a store's diffs in the
    /// order its rows were stored (see [`order`]).
    groups: Groups,
}

impl SharedTick {
    fn new() -> Self {
        Self {
            ready: ReadyQueue::new(),
            depth: 0,
            draining: false,
            groups: Groups::default(),
        }
    }

    /// Put `staged` in the queue in order, collecting the closures it
    /// supersedes for the caller to drop once the tick is unlocked.
    fn commit(&mut self, staged: &mut Vec<Staged>, displaced: &mut Vec<PendingRun>) {
        for op in staged.drain(..) {
            if let Some(old) = self.enqueue_locked(op.id, op.height, op.coalesce, op.run) {
                displaced.push(old);
            }
        }
    }

    /// Enqueue `run` for cell `id` at `height`. When `coalesce` is true and the
    /// cell is already queued, its previous op is dropped (last-write-wins).
    /// When false (a `no_coalesce` cell), every op is kept as a distinct
    /// arrival-ordered key so event semantics survive.
    ///
    /// Kept out of line so its two callers, [`enqueue`] and a batch's commit,
    /// share one copy with the queue's insert path inlined into it.
    #[inline(never)]
    fn enqueue_locked(
        &mut self,
        id: Uuid,
        height: u64,
        coalesce: bool,
        run: Box<dyn FnOnce() + Send>,
    ) -> Option<PendingRun> {
        self.ready.enqueue(id, height, coalesce, run)
    }

    /// Move every op at the current minimum height into `wave`, **grouped by
    /// cell id**, each group in arrival order; `false` if the frontier is empty.
    ///
    /// The parallel unit is a *cell*, not an op. Distinct groups are distinct
    /// cells at the same height — mathematically independent (see [`run_wave`]'s
    /// docs), safe to run concurrently. Ops **within** a group all target the
    /// same cell: a `no_coalesce` cell can have several ops queued here at once,
    /// and running its value-settle + fanout concurrently with itself would both
    /// reorder its events and invoke its subscribers on two threads at once. So
    /// a group must run sequentially in arrival order. A coalescing cell can
    /// also have terminal or event operations in its group.
    #[inline]
    fn pop_min_height_into(&mut self, wave: &mut Wave) -> bool {
        self.ready.pop_min_height_into(wave)
    }
}

static TICK: LazyLock<Mutex<SharedTick>> = LazyLock::new(|| Mutex::new(SharedTick::new()));

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
#[inline]
fn refresh_active(active: &AtomicBool, t: &SharedTick) {
    active.store(t.depth > 0 || t.draining, Ordering::Relaxed);
}

/// Signalled whenever the root drain is released, for a thread waiting in
/// [`quiesce`] for the root tick to go quiet.
static TICK_RELEASED: Condvar = Condvar::new();

/// One domain's tick: the root tick is the process-wide static, a child's is a
/// field of its [`DomainInner`]. Everything below operates on a `TickRef`, so
/// the root and child paths are the same code.
#[derive(Clone, Copy)]
struct TickRef<'a> {
    id: u64,
    queue: &'a Mutex<SharedTick>,
    active: &'a AtomicBool,
    released: &'a Condvar,
    child: Option<&'a Arc<DomainInner>>,
}

impl<'a> TickRef<'a> {
    fn of(domain: Option<&'a Arc<DomainInner>>) -> Self {
        domain.map_or_else(
            || Self {
                id: domain::ROOT_ID,
                queue: &TICK,
                active: &TICK_ACTIVE,
                released: &TICK_RELEASED,
                child: None,
            },
            |child| Self {
                id: child.id,
                queue: &child.tick,
                active: &child.active,
                released: &child.released,
                child: Some(child),
            },
        )
    }

    #[inline]
    fn refresh_active(self, t: &SharedTick) {
        refresh_active(self.active, t);
    }
}

/// The node's propagation height. Heights are maintained incrementally as
/// edges are added (see `crate::cell::raise_height`), so this is one atomic
/// load for a cell.
fn compute_height(node: &dyn DepNode) -> u64 {
    node.height()
}

/// Whether a batch is active on any thread for `domain`'s tick (`None` is
/// root). Cheap — one relaxed atomic load. This is the single check `notify`
/// pays on the synchronous hot path when no batch is open.
#[inline]
pub(crate) fn tick_active(domain: Option<&Arc<DomainInner>>) -> bool {
    domain.map_or_else(
        || TICK_ACTIVE.load(Ordering::Relaxed),
        |child| child.active.load(Ordering::Relaxed),
    )
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
/// terminal takes a later position in its cell group, so the value drains
/// first and the terminal after it. The queue preserves arrival order for
/// later operations too; completion only takes effect when its operation runs.
pub(crate) fn enqueue(
    domain: Option<&Arc<DomainInner>>,
    id: Uuid,
    node: &dyn DepNode,
    terminal: bool,
    run: Box<dyn FnOnce() + Send>,
) {
    let tick = TickRef::of(domain);
    // A write this thread makes inside its own open batch is staged, so no
    // drain can run it before the rest of that batch.
    let run = match stage(tick.id, id, node, terminal, run) {
        Ok(()) => return,
        Err(run) => run,
    };
    let mut guard = tick.queue.lock();
    // Defer only if a batch is open somewhere, or a drain is in flight (a
    // popped op's fanout must land in the current drain's queue, not cascade
    // eagerly). Otherwise run synchronously on this thread — the default
    // eager path. Re-validated here under the lock, so a stale `TICK_ACTIVE`
    // hint can't strand an op in a queue nobody will drain.
    if guard.depth == 0 && !guard.draining {
        drop(guard);
        let _scope = domain::propagate_if_in_use(domain);
        run();
        return;
    }
    let height = compute_height(node);
    let coalesce = !node.no_coalesce() && !terminal;
    let displaced = guard.enqueue_locked(id, height, coalesce, run);
    drop(guard);
    // Captured values can own Cells whose final Drop re-enters scheduler paths.
    // Never destroy a superseded closure while holding the process-wide tick.
    drop(displaced);
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
/// the shared worker pool is built lazily, so a process whose waves never cross it spawns
/// zero wave threads and pays zero idle cost.
// Rack emitter/state-frame calibration found cheap 64/128-cell waves were
// 5.7–7.7x faster sequentially. Keep wider waves eligible for compute-heavy
// callbacks; applications with expensive narrow waves can explicitly use 64.
const DEFAULT_WAVE_THRESHOLD: usize = 256;

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

#[cfg(not(target_arch = "wasm32"))]
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

/// Run one height's worth of ops, grouped by cell (see
/// [`SharedTick::pop_min_height_into`]). Distinct groups are distinct cells at
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
/// [`WAVE_THRESHOLD`] gets real parallelism on the dedicated shared pool.
/// Panics are caught per-op so one buggy callback can't strand or corrupt
/// unrelated work; every payload seen is returned for the caller to pick one to
/// re-raise.
///
/// A child domain's wave always runs sequentially on its drainer: children get
/// their parallelism from running beside each other, and staying off the
/// shared pool means a wave occupying it can never hold a child up. While any
/// child domain exists, a root wave marks each thread it runs on so root-cell
/// fanouts know they belong to a root drain (see [`domain::hand_off`]).
#[cfg(not(target_arch = "wasm32"))]
fn run_wave(wave: &mut Wave, tick: TickRef<'_>) -> Vec<Box<dyn std::any::Any + Send>> {
    fn run_ops(
        ops: impl Iterator<Item = Box<dyn FnOnce() + Send>>,
    ) -> Vec<Box<dyn std::any::Any + Send>> {
        ops.filter_map(|run| std::panic::catch_unwind(std::panic::AssertUnwindSafe(run)).err())
            .collect()
    }
    let groups = wave.ends.len();
    if tick.child.is_some() {
        wave.ends.clear();
        return run_ops(wave.runs.drain(..));
    }
    let handing_off = domain::in_use();
    let _root_wave = handing_off.then(domain::enter_root_wave);
    // Common/resting case: below the threshold, or parallelism disabled — run
    // sequentially and never touch the pool (so it stays unbuilt).
    let pool = (groups >= wave_threshold())
        .then(crate::executor::worker_pool)
        .flatten();
    let Some(pool) = pool else {
        wave.ends.clear();
        return run_ops(wave.runs.drain(..));
    };
    run_parallel(pool, wave, groups, handing_off)
}

#[cfg(not(target_arch = "wasm32"))]
#[cold]
fn run_parallel(
    pool: &rayon::ThreadPool,
    wave: &mut Wave,
    groups: usize,
    handing_off: bool,
) -> Vec<Box<dyn std::any::Any + Send>> {
    use rayon::prelude::*;
    let mut split = Vec::with_capacity(groups);
    let mut runs = wave.runs.drain(..);
    let mut start = 0;
    for &end in &wave.ends {
        split.push(
            runs.by_ref()
                .take(end.saturating_sub(start))
                .collect::<Vec<_>>(),
        );
        start = end;
    }
    drop(runs);
    wave.ends.clear();
    pool.install(|| {
        split
            .into_par_iter()
            .flat_map_iter(|group| {
                let _scope =
                    handing_off.then(|| (domain::propagate(None), domain::enter_root_wave()));
                group
                    .into_iter()
                    .filter_map(|run| {
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(run)).err()
                    })
                    .collect::<Vec<_>>()
            })
            .collect()
    })
}

/// wasm is single-threaded — no real parallelism to exploit, and no
/// cross-thread race to guard against either, but this module still needs to
/// compile and behave correctly there. Groups run in order, ops within each in arrival order.
#[cfg(target_arch = "wasm32")]
fn run_wave(wave: &mut Wave, tick: TickRef<'_>) -> Vec<Box<dyn std::any::Any + Send>> {
    let _root_wave = (tick.child.is_none() && domain::in_use()).then(domain::enter_root_wave);
    wave.ends.clear();
    wave.runs
        .drain(..)
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
///
/// `domain` selects the tick (`None` is root). A root drain that runs dry also
/// distributes the deliveries it produced for child domains, one unit per
/// child, before it releases. Every release wakes the threads waiting in
/// [`quiesce`] for that tick.
fn drain(domain: Option<&Arc<DomainInner>>) -> Option<Box<dyn std::any::Any + Send>> {
    let panicked = drain_waves(domain);
    if let Some(payload) = panicked.as_ref() {
        domain::caught_panic(domain, "propagation", payload.as_ref());
    }
    panicked
}

fn drain_waves(domain: Option<&Arc<DomainInner>>) -> Option<Box<dyn std::any::Any + Send>> {
    // A drain on this thread can nest inside another's wave (a child domain's
    // drain), so the buffer is taken rather than borrowed.
    let mut wave = WAVE_BUFFER.take();
    let panicked = drain_waves_into(domain, &mut wave);
    WAVE_BUFFER.set(wave);
    panicked
}

thread_local! {
    static WAVE_BUFFER: std::cell::Cell<Wave> = std::cell::Cell::new(Wave::default());
}

fn drain_waves_into(
    domain: Option<&Arc<DomainInner>>,
    wave: &mut Wave,
) -> Option<Box<dyn std::any::Any + Send>> {
    let tick = TickRef::of(domain);
    let _scope = domain::propagate_if_in_use(domain);
    let _suspended = SuspendedBatch::of(tick.id);
    let mut first_panic: Option<Box<dyn std::any::Any + Send>> = None;
    let mut waves = 0usize;
    loop {
        {
            let mut guard = tick.queue.lock();
            // Bounded rotation: after a stint's worth of waves, hand the drain
            // off *if another batch is still open* (`depth > 0`, so a close is
            // coming to reclaim it). This caps any single `batch()` call's
            // drain time, so a burst of concurrent batchers rotate the drain
            // among themselves instead of pinning the first claimant as the
            // sole drainer for the whole storm. The `depth > 0` check and the
            // release happen under this one lock, so it's strand-free: if the
            // last other batch just closed (`depth == 0`), we don't hand off —
            // there'd be no one to reclaim — and drain to completion instead.
            //
            // The queue must also be non-empty: only a close that finds work
            // claims the drain, and a root drain that released dry without
            // distributing its hand-offs would leave them with nobody to
            // deliver them.
            if waves >= DRAIN_STINT_WAVES && guard.depth > 0 && !guard.ready.is_empty() {
                guard.draining = false;
                tick.refresh_active(&guard);
                drop(guard);
                tick.released.notify_all();
                return first_panic;
            }
            if guard.pop_min_height_into(wave) {
                drop(guard);
            } else {
                // Under the lock, so two root drains' units reach each child
                // in drain order.
                let handed_off =
                    (tick.child.is_none() && domain::in_use()).then(domain::flush_root_outbox);
                guard.draining = false;
                tick.refresh_active(&guard);
                drop(guard);
                if let Some(handed_off) = handed_off {
                    handed_off.finish();
                }
                tick.released.notify_all();
                return first_panic;
            }
        }
        let panics = run_wave(wave, tick);
        if first_panic.is_none() {
            first_panic = panics.into_iter().next();
        }
        waves = waves.saturating_add(1);
    }
}

/// How many height-waves one thread drains before handing the drain off to
/// another open batch (see [`drain`]). Large enough that the common
/// single-threaded case (which drains a whole tick in far fewer waves and then
/// finds the queue empty) never hits it, small enough that under a many-thread
/// batch storm no single `batch()` call is pinned draining for long.
const DRAIN_STINT_WAVES: usize = 64;

struct NoCoalesceGuard;

impl Drop for NoCoalesceGuard {
    fn drop(&mut self) {
        NO_COALESCE_DEPTH.with(|depth| depth.set(depth.get().saturating_sub(1)));
    }
}

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
    NO_COALESCE_DEPTH.with(|d| d.set(d.get().saturating_add(1)));
    let _guard = NoCoalesceGuard;
    f()
}

/// Whether a [`no_coalesce`] construction scope is active on this thread. Read
/// by `Cell::new` to stamp a cell's coalescing policy at birth.
pub(crate) fn birth_no_coalesce() -> bool {
    NO_COALESCE_DEPTH.with(|d| d.get() > 0)
}

/// Run `f` as a single propagation batch.
///
/// Every `set`/`notify` on ANY thread while `f` runs is deferred to the one
/// shared tick instead of cascading. None of `f`'s own writes drains before
/// `f` (and any batch nested inside it on this thread) has fully returned.
/// Draining any earlier would let a diamond join fire on a partial set of
/// inputs, since the second input might not have arrived yet.
///
/// The batch is atomic to observers: the writes `f` makes on this thread are
/// staged and reach the tick together when the thread's outermost batch
/// closes, so no drain on another thread can run some of them while `f` is
/// still running.
///
/// A row `f` writes to a `CellMap` or `CellSet` is stored at once, so `f`
/// and every other thread read it back from the map before the batch closes.
/// Its diff is part of the batch. When another thread's open batch wrote the
/// same key, the two batches reach the tick together, once both have closed,
/// with that key's diffs in the order the rows were stored. The one that
/// closes first returns without waiting, and its writes are delivered when
/// the other closes.
///
/// Nested `batch` calls — genuinely nested on the same thread's call stack,
/// or a different thread racing this one — join the same shared tick rather
/// than opening a second one. Draining is claimed at a thread's *outermost*
/// batch close, by whichever such close
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
/// See the module docs for the deferred semantics.
///
/// The batch opens on the calling thread's ambient [`Domain`]: root unless the
/// call is inside a child's [`Domain::enter`] scope or one of its waves. A
/// write to a cell of another domain is not part of the batch: it takes
/// effect on that domain's tick at once.
pub fn batch<R>(f: impl FnOnce() -> R) -> R {
    match domain::ambient_child() {
        None => batch_in(None, f),
        Some(child) => batch_in(Some(&child), f),
    }
}

/// [`batch`] on `domain`'s tick (`None` is root).
pub(crate) fn batch_in<R>(domain: Option<&Arc<DomainInner>>, f: impl FnOnce() -> R) -> R {
    begin_batch(domain);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    let drain_panic = end_batch(domain);

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

/// Open one batch of `domain` on this thread.
fn begin_batch(domain: Option<&Arc<DomainInner>>) {
    let tick = TickRef::of(domain);
    open_batch(tick.id);
    let mut guard = tick.queue.lock();
    guard.depth = guard.depth.saturating_add(1);
    tick.refresh_active(&guard);
    drop(guard);
}

/// Close one batch of `domain` on this thread, and drain if this close claims
/// the drain. Returns the first panic a drained op raised.
///
/// A thread's OUTERMOST batch commits everything it staged in one step under
/// the lock, so a drain sees the whole batch or none of it, and then decides
/// whether to drain: only if no drain is already in flight and there is
/// pending work. The empty-check and the `draining = true` claim happen
/// together under the lock, which is what makes the hand-off strand-free.
///
/// A batch that stored rows another unfinished writer's rows precede or
/// follow belongs to a commit group (see [`order`]). It commits with the
/// group: when it is not the last member to finish it leaves its writes with
/// the group and returns, and the last member puts all of them in the tick in
/// this same step.
fn end_batch(domain: Option<&Arc<DomainInner>>) -> Option<Box<dyn std::any::Any + Send>> {
    let tick = TickRef::of(domain);
    let mut closed = close_batch(tick.id);
    let outermost = closed.is_some();
    let mut displaced = Vec::new();
    let claimed_drain = {
        let mut guard = tick.queue.lock();
        guard.depth = guard.depth.saturating_sub(1);
        if let Some(closed) = closed.as_mut() {
            if let Some(writer) = closed.writer.as_ref() {
                guard
                    .groups
                    .finish(writer, true, &mut closed.staged, &mut closed.claims);
            }
            guard.commit(&mut closed.staged, &mut displaced);
        }
        let claim = outermost && !guard.draining && !guard.ready.is_empty();
        if claim {
            guard.draining = true;
        }
        tick.refresh_active(&guard);
        drop(guard);
        claim
    };
    // A superseded closure can own a cell whose drop re-enters the scheduler.
    drop(displaced);
    if let Some(mut closed) = closed {
        closed.claims.drain(..).for_each(Claim::release);
        if closed.claims.capacity() <= SPARE_STAGING_LIMIT {
            SPARE_CLAIMS.set(closed.claims);
        }
        if let Some(writer) = closed.writer {
            Writer::retire(writer);
        }
        if closed.staged.capacity() <= SPARE_STAGING_LIMIT {
            SPARE_STAGING.set(closed.staged);
        }
    }

    if claimed_drain { drain(domain) } else { None }
}

/// The deliveries `writer`'s thread made outside a batch have returned, and
/// other writers joined it meanwhile. Leave their group, and when that
/// leaves nobody open, put what the group held in `domain`'s tick and drain
/// it.
fn finish_delivering(domain: Option<&Arc<DomainInner>>, writer: &Arc<Writer>) {
    let tick = TickRef::of(domain);
    let (mut staged, mut claims, mut displaced) = (Vec::new(), Vec::new(), Vec::new());
    let claimed_drain = {
        let mut guard = tick.queue.lock();
        guard.groups.finish(writer, false, &mut staged, &mut claims);
        let committed = !staged.is_empty();
        guard.commit(&mut staged, &mut displaced);
        let claim = committed && !guard.draining;
        if claim {
            guard.draining = true;
        }
        tick.refresh_active(&guard);
        drop(guard);
        claim
    };
    drop(displaced);
    claims.into_iter().for_each(Claim::release);
    if claimed_drain {
        // A panic in the drain has been reported; nobody here resumes it.
        drop(drain(domain));
    }
}

/// Block until `domain`'s tick has no drain in flight and no queued op,
/// draining it on this thread whenever nobody else is. Every write committed
/// to the tick before the call has then been propagated. Returns early for a
/// closed child domain.
fn quiesce(domain: Option<&Arc<DomainInner>>) {
    let tick = TickRef::of(domain);
    loop {
        let mut guard = tick.queue.lock();
        if tick.child.is_some_and(|child| child.is_closed()) {
            return;
        }
        if guard.draining {
            tick.released.wait(&mut guard);
            continue;
        }
        if guard.ready.is_empty() {
            return;
        }
        guard.draining = true;
        tick.refresh_active(&guard);
        drop(guard);
        drop(drain(domain));
    }
}
