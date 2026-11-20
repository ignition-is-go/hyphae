//! The settle core: pipelined tickets.
//!
//! Every event set gets a ticket in arrival order. Each node fires its
//! pending tickets in increasing order, and fires ticket `t` only once every
//! input has promised never to deliver a ticket at or below `t` again. Ticket
//! `t + 1` can settle at low heights while ticket `t` is still settling above
//! it. There is no settle lock.
//!
//! The design is `docs/settle-epochs.md` (sections 3 to 7). A side read with
//! `get()` reads the latest finished value, except inside a family row build,
//! where it reads at the build's ticket. A callback of an owned subscription runs at its owner's
//! fire, so operator state lives with the node it computes.

use std::{
    any::Any,
    cell::RefCell,
    num::NonZeroU64,
    panic::Location,
    sync::{
        Arc, LazyLock, OnceLock, Weak,
        atomic::{AtomicI64, AtomicU8, AtomicU64, Ordering},
    },
};

use crossbeam::queue::SegQueue;
use parking_lot::Mutex;
use rustc_hash::{FxHashMap, FxHashSet};
use uuid::Uuid;

use crate::traits::DepNode;

mod dump;
mod edges;
mod pool;
mod watermark;

pub(crate) use edges::unbound_owned;
pub use edges::{new_ticket, unbound_sites};
pub use pool::set_workers;

/// Marks an event set that must settle alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SettleKey {
    /// No constraint.
    Shared,
    /// Seeded only after every earlier ticket has completed. Nothing after it
    /// is seeded until it has completed.
    Barrier,
}

/// Names a write and everything it causes.
///
/// A writer attaches an epoch to an event set with [`batch_with`], [`submit`],
/// or [`enqueue`]. While that set's ticket is delivered, [`current_epoch`]
/// returns the epoch in every callback, and an event set that a callback
/// queues carries it on unless the callback passes its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Epoch(NonZeroU64);

static NEXT_EPOCH: AtomicU64 = AtomicU64::new(1);

/// Set in an epoch from [`Epoch::scoped`], so that every other epoch skips
/// the scope table.
const SCOPED: u64 = 1 << 63;

impl Epoch {
    /// Allocate an epoch that no other call has returned.
    #[must_use]
    pub fn next() -> Self {
        let raw = NEXT_EPOCH.fetch_add(1, Ordering::Relaxed) & !SCOPED;
        Self(NonZeroU64::new(raw).unwrap_or(NonZeroU64::MIN))
    }

    /// Allocate an epoch and call `finished` once it is over.
    ///
    /// The epoch is over when the returned scope has been dropped and every
    /// ticket that carries the epoch has completed, including the tickets
    /// that callbacks queued while one of them was delivered.
    ///
    /// `finished` runs once, on the thread that drops the scope or on the
    /// thread that completes the last ticket. There it is a callback: it must
    /// not block, [`current_epoch`] returns `None`, and a write it makes
    /// becomes a new ticket.
    pub fn scoped(finished: impl FnOnce() + Send + 'static) -> EpochScope {
        let epoch = Self(Self::next().0 | SCOPED);
        SCOPES.lock().insert(
            epoch,
            Scope {
                carriers: 1,
                finished: Box::new(finished),
            },
        );
        EpochScope(epoch)
    }

    const fn raw(self) -> u64 {
        self.0.get()
    }

    const fn from_raw(raw: u64) -> Option<Self> {
        match NonZeroU64::new(raw) {
            Some(raw) => Some(Self(raw)),
            None => None,
        }
    }
}

/// Keeps an epoch from [`Epoch::scoped`] open. Drop it when the writer has
/// made its last write under the epoch.
#[must_use = "dropping the scope lets the epoch finish"]
#[derive(Debug)]
pub struct EpochScope(Epoch);

impl EpochScope {
    /// The epoch to attach to writes.
    #[must_use]
    pub const fn epoch(&self) -> Epoch {
        self.0
    }
}

impl Drop for EpochScope {
    fn drop(&mut self) {
        let Some(finished) = release(self.0, 1) else {
            return;
        };
        if std::thread::panicking() {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(finished));
        } else {
            finished();
        }
    }
}

struct Scope {
    carriers: u64,
    finished: Box<dyn FnOnce() + Send>,
}

static SCOPES: LazyLock<Mutex<FxHashMap<Epoch, Scope>>> = LazyLock::new(Mutex::default);

fn carry(epoch: Option<Epoch>) {
    if let Some(epoch) = epoch
        && epoch.0.get() & SCOPED != 0
        && let Some(scope) = SCOPES.lock().get_mut(&epoch)
    {
        scope.carriers = scope.carriers.saturating_add(1);
    }
}

fn release(epoch: Epoch, carriers: u64) -> Option<Box<dyn FnOnce() + Send>> {
    if epoch.0.get() & SCOPED == 0 {
        return None;
    }
    let mut scopes = SCOPES.lock();
    let scope = scopes.get_mut(&epoch)?;
    scope.carriers = scope.carriers.saturating_sub(carriers);
    if scope.carriers > 0 {
        return None;
    }
    let scope = scopes.remove(&epoch);
    drop(scopes);
    scope.map(|scope| scope.finished)
}

// ============================================================================
// Nodes
// ============================================================================

/// A node as the scheduler sees it.
///
/// Public only because it appears in [`DepNode::add_dependent`]. It is not
/// stable API.
#[doc(hidden)]
pub trait Node: Send + Sync {
    fn node_id(&self) -> Uuid;
    /// `1 + max(height of each owned dependency)`; an input is 0.
    fn node_height(&self) -> u64;
    /// The scheduler state of this node.
    fn node_core(&self) -> &Arc<NodeCore>;
    /// Deliver what the deliveries of `ticket` collected: store the value
    /// and tell subscribers.
    fn emit(&self, ticket: u64);
    /// Mark this node's cached height stale.
    fn bump_height_epoch(&self);
    /// The nodes whose height depends on this one. Dead entries are pruned.
    fn node_dependents(&self) -> Vec<Weak<dyn Node>>;
    /// Where the node was created, for diagnostics.
    fn node_caller(&self) -> Option<&'static Location<'static>> {
        None
    }
}

/// One typed write, queued until its ticket is seeded.
pub(crate) trait Write: Send {
    fn apply(self: Box<Self>);
}

/// No ticket.
const NONE: u64 = u64::MAX;

/// Where a delivery sits among the deliveries of one ticket to one node: the
/// order a one-ticket settle produces them in. Lower height first, then the
/// node created first, then the order that node produced them in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Stamp {
    height: u64,
    node: u64,
    index: u64,
}

impl Stamp {
    const SEED: Self = Self {
        height: 0,
        node: 0,
        index: 0,
    };
}

pub(crate) type Item = Box<dyn FnOnce() + Send>;

struct Delivery {
    ticket: u64,
    stamp: Stamp,
    item: Item,
    /// The address of the node whose fire pushed it, or 0 for a seed or a
    /// backfill. A gated edge reads it.
    from: usize,
}

struct Pending {
    /// Sorted by ticket; arrival order within a ticket.
    items: Vec<Delivery>,
    /// The ticket being fired, or [`NONE`].
    inflight: u64,
    /// `p(n)`: the last ticket fired, or [`NONE`] before the first.
    processed: u64,
}

impl Pending {
    fn min(&self) -> u64 {
        self.items
            .first()
            .map_or(self.inflight, |first| first.ticket.min(self.inflight))
    }

    /// True when `ticket` may still be pushed here: the node has not fired
    /// it, and has not promised past it.
    const fn accepts(&self, ticket: u64, watermark: u64) -> bool {
        (self.processed == NONE || self.processed < ticket) && watermark <= ticket
    }
}

const IDLE: u8 = 0;
const CLAIMED: u8 = 1;
const DIRTY: u8 = 2;

static NODE_SEQ: AtomicU64 = AtomicU64::new(1);
/// Bumped whenever an edge is added.
static EDGE_GEN: AtomicU64 = AtomicU64::new(0);

/// The scheduler state every node carries.
#[doc(hidden)]
pub struct NodeCore {
    seq: u64,
    pending: Mutex<Pending>,
    /// `w(n)`: this node never again delivers a ticket below it.
    watermark: AtomicU64,
    /// [`IDLE`], or claimed by one thread that is evaluating or firing it.
    sched: AtomicU8,
    /// Nodes that wait for this one to fire.
    parked: Mutex<Vec<Weak<dyn Node>>>,
    /// The nodes that have a declared edge into this one.
    inputs: Mutex<Vec<Weak<Self>>>,
    /// The nodes with a declared edge from this one.
    outputs: Mutex<Vec<Weak<Self>>>,
    /// Gated in-edges `(src, gate)`: `src` counts toward the fire rule at a
    /// ticket only when `gate`, an input, delivered that ticket here, and
    /// `src` pushes here only at such a ticket. Not in `inputs`, so they add
    /// nothing to `in_w` otherwise.
    gated: Mutex<Vec<(Weak<Self>, Weak<Self>)>>,
    /// The quiet certificate (`docs/settle-epochs.md` section 4).
    mark: AtomicU8,
    /// Where the node was created, for diagnostics.
    caller: OnceLock<&'static Location<'static>>,
}

impl NodeCore {
    pub(crate) fn new_arc() -> Arc<Self> {
        let core = Arc::new(Self {
            seq: NODE_SEQ.fetch_add(1, Ordering::Relaxed),
            pending: Mutex::new(Pending {
                items: Vec::new(),
                inflight: NONE,
                processed: NONE,
            }),
            watermark: AtomicU64::new(0),
            sched: AtomicU8::new(IDLE),
            parked: Mutex::new(Vec::new()),
            inputs: Mutex::new(Vec::new()),
            outputs: Mutex::new(Vec::new()),
            gated: Mutex::new(Vec::new()),
            mark: AtomicU8::new(watermark::NOT_QUIET),
            caller: OnceLock::new(),
        });
        ROW_BUILD.with_borrow_mut(|build| {
            if let Some(build) = build.as_mut() {
                build.created.push(Arc::downgrade(&core));
            }
        });
        core
    }

    /// Declare an edge from `input` into this node. The node's ancestry
    /// grows, so its quiet certificate and its descendants' are cleared.
    pub(crate) fn add_input(self: &Arc<Self>, input: &Arc<Self>) {
        if Arc::ptr_eq(self, input) {
            return;
        }
        {
            let mut inputs = self.inputs.lock();
            inputs.retain(|existing| existing.strong_count() > 0);
            let address = Arc::as_ptr(input);
            if inputs.iter().any(|existing| existing.as_ptr() == address) {
                return;
            }
            inputs.push(Arc::downgrade(input));
        }
        {
            let mut outputs = input.outputs.lock();
            outputs.retain(|existing| existing.strong_count() > 0);
            outputs.push(Arc::downgrade(self));
        }
        EDGE_GEN.fetch_add(1, Ordering::Release);
        watermark::invalidate(self, watermark::Cause::Edge);
    }

    pub(crate) fn set_caller(&self, caller: Option<&'static Location<'static>>) {
        if let Some(caller) = caller {
            let _ = self.caller.set(caller);
        }
    }

    /// Declare a gated edge from `src` into this node, gated by `gate`: see
    /// [`NodeCore::gated`]. `gate` must also be an input.
    pub(crate) fn add_gated_input(self: &Arc<Self>, src: &Arc<Self>, gate: &Arc<Self>) {
        self.add_input(gate);
        {
            let mut gated = self.gated.lock();
            gated.retain(|(src, gate)| src.strong_count() > 0 && gate.strong_count() > 0);
            if gated
                .iter()
                .any(|(s, g)| s.as_ptr() == Arc::as_ptr(src) && g.as_ptr() == Arc::as_ptr(gate))
            {
                return;
            }
            gated.push((Arc::downgrade(src), Arc::downgrade(gate)));
        }
        // Not an output of `src`: clearing and validation follow ungated
        // edges only, so a seed above the family node leaves this node's
        // quiet mark alone. It can reach this node only through `gate`,
        // whose delivery clears the mark (`docs/settle-epochs-model/
        // quiet_model.rs`, GATE=1).
        EDGE_GEN.fetch_add(1, Ordering::Release);
        watermark::invalidate(self, watermark::Cause::Edge);
    }

    /// The gated sources open at `ticket`: those whose gate delivered it.
    fn open_gates(&self, ticket: u64, items: &[Delivery]) -> Vec<Arc<Self>> {
        let gated = self.gated.lock();
        if gated.is_empty() {
            return Vec::new();
        }
        gated
            .iter()
            .filter(|(_, gate)| {
                let gate = gate.as_ptr().addr();
                items.iter().any(|d| d.ticket == ticket && d.from == gate)
            })
            .filter_map(|(src, _)| src.upgrade())
            .collect()
    }

    /// No delivery pending or in flight.
    fn idle(&self) -> bool {
        let pending = self.pending.lock();
        pending.items.is_empty() && pending.inflight == NONE
    }

    /// Push `item` at `ticket`. Fails when the node has fired the ticket or
    /// promised past it, which would break per-node order.
    fn push(
        &self,
        ticket: u64,
        stamp: Stamp,
        arrival: bool,
        item: Item,
        from: usize,
    ) -> Result<(), Item> {
        let mut pending = self.pending.lock();
        if !pending.accepts(ticket, self.watermark.load(Ordering::Acquire)) {
            return Err(item);
        }
        #[cfg(debug_assertions)]
        if from != 0 {
            for (src, gate) in self.gated.lock().iter() {
                if src.as_ptr().addr() == from {
                    let gate = gate.as_ptr().addr();
                    assert!(
                        pending.items.iter().any(|d| d.ticket == ticket && d.from == gate),
                        "hyphae: gated edge: a push at ticket {ticket} whose gate did not deliver it"
                    );
                }
            }
        }
        let at = pending.items.partition_point(|d| d.ticket <= ticket);
        let stamp = match at.checked_sub(1).and_then(|i| pending.items.get(i)) {
            Some(last) if arrival && last.ticket == ticket => stamp.max(last.stamp),
            _ => stamp,
        };
        pending.items.insert(
            at,
            Delivery {
                ticket,
                stamp,
                item,
                from,
            },
        );
        drop(pending);
        slot(ticket).count.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }

    /// `p(n)`: the last ticket this node fired, or `None` before the first.
    pub(crate) fn processed(&self) -> Option<u64> {
        let processed = self.pending.lock().processed;
        (processed != NONE).then_some(processed)
    }

    fn can_join(&self, ticket: u64) -> bool {
        self.pending
            .lock()
            .accepts(ticket, self.watermark.load(Ordering::Acquire))
    }

    fn wake_parked(&self) {
        let parked = std::mem::take(&mut *self.parked.lock());
        for node in parked {
            if let Some(node) = node.upgrade() {
                poke(&node);
            }
        }
    }
}

/// A node dropped with deliveries still pending never fires them. Count them
/// as consumed so their tickets complete, and wake the nodes parked on it.
impl Drop for NodeCore {
    fn drop(&mut self) {
        let items = std::mem::take(&mut self.pending.get_mut().items);
        let parked = std::mem::take(self.parked.get_mut());
        for delivery in items {
            drop(delivery.item);
            consumed(delivery.ticket, 1);
        }
        for node in parked {
            if let Some(node) = node.upgrade() {
                poke(&node);
            }
        }
    }
}

// ============================================================================
// Tickets
// ============================================================================

const RING: u64 = 1 << 16;

struct Slot {
    count: AtomicI64,
    /// `ticket + 1` once the ticket completed.
    done: AtomicU64,
    epoch: AtomicU64,
    /// The scoped event sets this ticket carries.
    carriers: AtomicU64,
    panic: Mutex<Option<(Stamp, Box<dyn Any + Send>)>>,
}

static SLOTS: LazyLock<Box<[Slot]>> = LazyLock::new(|| {
    (0..RING)
        .map(|_| Slot {
            count: AtomicI64::new(0),
            done: AtomicU64::new(0),
            epoch: AtomicU64::new(0),
            carriers: AtomicU64::new(0),
            panic: Mutex::new(None),
        })
        .collect()
});

fn slot(ticket: u64) -> &'static Slot {
    let index = usize::try_from(ticket % RING).unwrap_or_default();
    #[allow(clippy::indexing_slicing, reason = "the index is below the ring size")]
    &SLOTS[index]
}

/// `S`: every ticket below it has been seeded.
static SEEDED: AtomicU64 = AtomicU64::new(0);
/// `C`: every ticket below it has completed.
static COMPLETED: AtomicU64 = AtomicU64::new(0);

fn is_done_slot(ticket: u64) -> bool {
    slot(ticket).done.load(Ordering::Acquire) == ticket.wrapping_add(1)
}

fn is_complete(ticket: u64) -> bool {
    COMPLETED.load(Ordering::Acquire) > ticket || is_done_slot(ticket)
}

/// Keep the panic a one-ticket settle would have raised first: the one from
/// the lowest stamp.
fn record_panic(ticket: u64, stamp: Stamp, payload: Box<dyn Any + Send>) {
    let mut held = slot(ticket).panic.lock();
    let earlier = held.as_ref().is_none_or(|(kept, _)| stamp < *kept);
    let dropped = if earlier {
        held.replace((stamp, payload)).map(|(_, payload)| payload)
    } else {
        Some(payload)
    };
    drop(held);
    if dropped.is_some() {
        log::error!("hyphae: more than one callback panicked in ticket {ticket}");
    }
}

fn consumed(ticket: u64, deliveries: i64) {
    if slot(ticket).count.fetch_sub(deliveries, Ordering::AcqRel) == deliveries {
        complete(ticket);
    }
}

fn complete(ticket: u64) {
    let slot = slot(ticket);
    let epoch = Epoch::from_raw(slot.epoch.load(Ordering::Acquire));
    let carriers = slot.carriers.load(Ordering::Acquire);
    // An epoch that this ticket finishes is over before the ticket counts as
    // complete, so a writer or `flush` that waits on it sees `finished` run.
    if let Some(epoch) = epoch
        && carriers > 0
        && let Some(finished) = release(epoch, carriers)
    {
        detached(finished);
    }
    slot.done.store(ticket.wrapping_add(1), Ordering::Release);
    loop {
        let c = COMPLETED.load(Ordering::Acquire);
        if c >= SEEDED.load(Ordering::Acquire) || !is_done_slot(c) {
            break;
        }
        let _ =
            COMPLETED.compare_exchange(c, c.saturating_add(1), Ordering::AcqRel, Ordering::Acquire);
    }
    pool::notify();
    if !INBOX.is_empty() || GATE.lock().is_some() {
        admit_drain();
    }
}

/// Run `f` as a callback outside any ticket: its writes become new tickets
/// and it never waits.
fn detached(f: impl FnOnce()) {
    let previous = CTX.with_borrow_mut(|ctx| ctx.replace(Ctx::Detached));
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    CTX.with_borrow_mut(|ctx| *ctx = previous);
    if outcome.is_err() {
        log::error!("hyphae: an epoch's finished callback panicked");
    }
}

// ============================================================================
// Thread context
// ============================================================================

enum Phase {
    /// Running the node's deliveries: a write to the node itself is its
    /// value for this ticket.
    Collect,
    /// Telling subscribers: a write to the node itself is feedback.
    Fanout,
}

struct Firing {
    ticket: u64,
    core: Arc<NodeCore>,
    /// The node that pushed the delivery now running, if not this one.
    origin: Option<Arc<NodeCore>>,
    stamp: Stamp,
    phase: Phase,
    touched: Vec<Weak<dyn Node>>,
}

enum Ctx {
    Seeding {
        ticket: u64,
        index: u64,
        touched: Vec<Weak<dyn Node>>,
    },
    Firing(Firing),
    /// A callback outside any ticket.
    Detached,
}

/// What a writer does after it has queued an event set.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Mode {
    /// Wait until the ticket has completed, helping.
    Wait,
    /// Help until the ticket completes or no work is ready.
    Submit,
    /// Leave it queued.
    Enqueue,
}

struct Building {
    depth: u32,
    set: EventSet,
}

type DeclaredCache = FxHashMap<(usize, usize), (u64, bool)>;

thread_local! {
    static MODE: std::cell::Cell<Mode> = const { std::cell::Cell::new(Mode::Wait) };
    static CTX: RefCell<Option<Ctx>> = const { RefCell::new(None) };
    static BUILDING: RefCell<Option<Building>> = const { RefCell::new(None) };
    static DECLARED: RefCell<DeclaredCache> = RefCell::new(FxHashMap::default());
    static ROW_BUILD: RefCell<Option<RowBuild>> = const { RefCell::new(None) };
}

// ============================================================================
// Family row builds (`docs/settle-epochs.md` section 6, "Build inside the
// ticket")
// ============================================================================

/// A row build open on this thread, inside the fire of ticket `ticket`.
struct RowBuild {
    ticket: u64,
    /// Every node created by the build.
    created: Vec<Weak<NodeCore>>,
    /// Run when the build closes, after the created nodes promise past
    /// `ticket`: each pushes the tickets above `ticket` that a subscription
    /// made during the build missed.
    backfills: Vec<Box<dyn FnOnce()>>,
}

/// `C`: every ticket below it has completed. A versioned read never asks for
/// a ticket below it.
pub(crate) fn completed_through() -> u64 {
    COMPLETED.load(Ordering::Acquire)
}

/// The ticket a row build on this thread reads at: reads and subscribe
/// replays return each node's value at this ticket.
pub(crate) fn build_ticket() -> Option<u64> {
    ROW_BUILD.with_borrow(|build| build.as_ref().map(|build| build.ticket))
}

/// Run `backfill` when the row build open on this thread closes.
pub(crate) fn on_build_close(backfill: Box<dyn FnOnce()>) {
    ROW_BUILD.with_borrow_mut(|build| {
        if let Some(build) = build.as_mut() {
            build.backfills.push(backfill);
        }
    });
}

/// Build family rows inside the fire of `firing` at the ticket it fires.
///
/// Every value `f` reads, and every subscribe replay, is the value at that
/// ticket `N`. When `f` returns, every node it created has processed `N` and
/// promises `N + 1`, and every subscription it made is sent each ticket
/// above `N` that its source had already fired, at that ticket, through its
/// owner. A nested call joins the outer build.
///
/// Outside the family's fire, as when a reader is constructed, `f` is plain
/// construction and reads the latest values, like any node built there. A
/// demand change always reaches the family's fire, because the readers' key
/// subscriptions are bound to the family node.
///
/// # Panics
///
/// In debug builds, when a node the build created has an input with no
/// declared path into `family`: the family's anchor edges must cover every
/// input its rows read.
pub(crate) fn build_rows<R>(family: &Arc<NodeCore>, f: impl FnOnce() -> R) -> R {
    if build_ticket().is_some() {
        return f();
    }
    let ticket = CTX.with_borrow(|ctx| match ctx {
        Some(Ctx::Firing(fire)) if Arc::ptr_eq(&fire.core, family) => Some(fire.ticket),
        _ => None,
    });
    let Some(ticket) = ticket else {
        return f();
    };
    ROW_BUILD.with_borrow_mut(|build| {
        *build = Some(RowBuild {
            ticket,
            created: Vec::new(),
            backfills: Vec::new(),
        });
    });
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    let Some(build) = ROW_BUILD.with_borrow_mut(Option::take) else {
        unreachable!("the row build is open until here");
    };
    let created: Vec<Arc<NodeCore>> = build.created.iter().filter_map(Weak::upgrade).collect();
    #[cfg(debug_assertions)]
    assert_anchored(family, &created);
    for core in &created {
        let mut pending = core.pending.lock();
        debug_assert!(
            pending.min() > ticket,
            "hyphae: family rule: a row node holds ticket {} at its build ticket {ticket}",
            pending.min()
        );
        if pending.processed == NONE || pending.processed < ticket {
            pending.processed = ticket;
        }
        core.watermark
            .fetch_max(ticket.saturating_add(1).min(pending.min()), Ordering::AcqRel);
    }
    for backfill in build.backfills {
        backfill();
    }
    match outcome {
        Ok(value) => value,
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

/// The anchor rule: every input a row build reads is covered by the family
/// node's anchor edges. An input is covered when it reaches the family node
/// along declared edges, or when every one of its own inputs is covered.
/// Then a ticket above the build ticket that the input fired is still
/// pending on the way to the family node.
#[cfg(debug_assertions)]
fn assert_anchored(family: &Arc<NodeCore>, created: &[Arc<NodeCore>]) {
    fn covered(
        node: &Arc<NodeCore>,
        family: &Arc<NodeCore>,
        known: &mut FxHashMap<usize, bool>,
    ) -> bool {
        let address = Arc::as_ptr(node).addr();
        if let Some(known) = known.get(&address) {
            return *known;
        }
        // A cycle reads as covered while it is being decided.
        known.insert(address, true);
        let inputs: Vec<Arc<NodeCore>> =
            node.inputs.lock().iter().filter_map(Weak::upgrade).collect();
        let answer = declared(node, family)
            || (!inputs.is_empty() && inputs.iter().all(|input| covered(input, family, known)));
        known.insert(address, answer);
        answer
    }
    let mut known: FxHashMap<usize, bool> = created
        .iter()
        .map(|core| (Arc::as_ptr(core).addr(), true))
        .collect();
    for core in created {
        let inputs: Vec<Arc<NodeCore>> =
            core.inputs.lock().iter().filter_map(Weak::upgrade).collect();
        for input in inputs {
            assert!(
                covered(&input, family, &mut known),
                "hyphae: family rule: a row reads a node with no anchor edge into its family"
            );
        }
    }
}

/// Push `item` to `owner` at `ticket`, a ticket above the row build that
/// subscribed it. The ticket cannot have completed: the family node that
/// builds holds the build ticket and has a static edge from every input
/// that delivered it.
pub(crate) fn backfill(owner: &Arc<dyn Node>, ticket: u64, item: Item) {
    let core = owner.node_core();
    let stamp = Stamp {
        height: owner.node_height(),
        node: 0,
        index: 0,
    };
    let pushed = core.push(ticket, stamp, false, item, 0).is_ok();
    debug_assert!(
        pushed,
        "hyphae: promise safety: a backfill at ticket {ticket} reached a node past it"
    );
    debug_assert!(
        slot(ticket).done.load(Ordering::Acquire) != ticket.wrapping_add(1),
        "hyphae: family rule: backfill of ticket {ticket} after it completed"
    );
    watermark::invalidate_if_marked(core);
    poke(owner);
}

/// Run `item` in the fire of `node`: at the ticket this thread delivers
/// when a declared edge allows it, otherwise as a new ticket.
pub(crate) fn run_in_fire(node: &Arc<dyn Node>, item: Item) {
    if collecting(node.as_ref()) {
        item();
        return;
    }
    let joins = CTX.with_borrow(|ctx| match ctx {
        Some(Ctx::Firing(firing)) => {
            declared(&firing.core, node.node_core()) && node.node_core().can_join(firing.ticket)
        }
        _ => false,
    });
    if joins {
        push_current(node, false, item);
        return;
    }
    submit_write(
        SettleKey::Shared,
        Box::new(InFire {
            node: Arc::downgrade(node),
            item,
        }),
    );
}

struct InFire {
    node: Weak<dyn Node>,
    item: Item,
}

impl Write for InFire {
    fn apply(self: Box<Self>) {
        if let Some(node) = self.node.upgrade() {
            push_current(&node, false, self.item);
        }
    }
}

/// The ticket this thread is delivering, if any.
pub(crate) fn current_ticket() -> Option<u64> {
    CTX.with_borrow(|ctx| match ctx {
        Some(Ctx::Seeding { ticket, .. }) => Some(*ticket),
        Some(Ctx::Firing(firing)) => Some(firing.ticket),
        _ => None,
    })
}

/// The epoch of the ticket now being delivered on this thread, or `None`
/// outside a ticket and for a ticket without one. See [`Epoch`].
#[must_use]
pub fn current_epoch() -> Option<Epoch> {
    current_ticket().and_then(|ticket| Epoch::from_raw(slot(ticket).epoch.load(Ordering::Acquire)))
}

/// True on a thread that is delivering a ticket: inside a subscriber
/// callback. A write made there never waits.
#[must_use]
pub fn is_settling() -> bool {
    CTX.with_borrow(Option::is_some)
}

// ============================================================================
// Stats
// ============================================================================

/// Process-wide totals since start. See [`stats`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SettleStats {
    /// Event sets submitted.
    pub event_sets: u64,
    /// Tickets seeded.
    pub tickets: u64,
    /// Fanouts delivered.
    pub fires: u64,
    /// Writers that waited for their ticket.
    pub waits: u64,
    /// Writes made during a ticket that became a later ticket.
    pub deferred_writes: u64,
    /// Nanoseconds spent firing nodes.
    pub fire_nanos: u64,
    /// The longest single node fire, in nanoseconds.
    pub max_fire_nanos: u64,
    /// Tickets that [`flush`] took.
    pub flush_sets: u64,
    /// Node fires.
    pub node_fires: u64,
    /// Node fires on pool threads.
    pub pool_fires: u64,
    /// The most tickets seeded and not completed at one time.
    pub max_in_flight: u64,
    /// Watermark walks that a readiness check started.
    pub walks: u64,
    /// Nodes those walks visited.
    pub walk_nodes: u64,
    /// Times a node parked on a blocker.
    pub parks: u64,
    /// `get()` and subscribe-time reads made inside a callback.
    pub side_reads: u64,
    /// Callback writes to a node with no declared edge from the writer.
    pub undeclared_writes: u64,
    /// Owned subscriptions whose callback runs at the source's fire because
    /// it could not be bound to its owner.
    pub unbound_owned: u64,
    /// Readiness checks of an input answered by its quiet certificate.
    pub quiet_hits: u64,
    /// Nodes certified quiet.
    pub validated: u64,
    /// Quiet certificates cleared by seeds and new edges.
    pub invalidated: u64,
    /// Back edges a watermark walk ignored: cycles in the declared graph.
    pub cycles: u64,
}

static EVENT_SETS: AtomicU64 = AtomicU64::new(0);
static TICKETS: AtomicU64 = AtomicU64::new(0);
static FIRES: AtomicU64 = AtomicU64::new(0);
static WAITS: AtomicU64 = AtomicU64::new(0);
static DEFERRED_WRITES: AtomicU64 = AtomicU64::new(0);
static FIRE_NANOS: AtomicU64 = AtomicU64::new(0);
static MAX_FIRE_NANOS: AtomicU64 = AtomicU64::new(0);
static INTERVAL_MAX_FIRE_NANOS: AtomicU64 = AtomicU64::new(0);
static FLUSH_SETS: AtomicU64 = AtomicU64::new(0);
static NODE_FIRES: AtomicU64 = AtomicU64::new(0);
static POOL_FIRES: AtomicU64 = AtomicU64::new(0);
static MAX_IN_FLIGHT: AtomicU64 = AtomicU64::new(0);
static WALKS: AtomicU64 = AtomicU64::new(0);
static WALK_NODES: AtomicU64 = AtomicU64::new(0);
static PARKS: AtomicU64 = AtomicU64::new(0);
static SIDE_READS: AtomicU64 = AtomicU64::new(0);
static UNDECLARED: AtomicU64 = AtomicU64::new(0);
static UNBOUND_OWNED: AtomicU64 = AtomicU64::new(0);
static QUIET_HITS: AtomicU64 = AtomicU64::new(0);
static CYCLES: AtomicU64 = AtomicU64::new(0);
/// Sampled back edges, `(from site, to site) -> count`.
static CYCLE_SITES: LazyLock<Mutex<FxHashMap<(String, String), u64>>> = LazyLock::new(Mutex::default);
static VALIDATED: AtomicU64 = AtomicU64::new(0);
static INVALIDATED: AtomicU64 = AtomicU64::new(0);

/// Read the process-wide settle counters.
#[must_use]
pub fn stats() -> SettleStats {
    SettleStats {
        event_sets: EVENT_SETS.load(Ordering::Relaxed),
        tickets: TICKETS.load(Ordering::Relaxed),
        fires: FIRES.load(Ordering::Relaxed),
        waits: WAITS.load(Ordering::Relaxed),
        deferred_writes: DEFERRED_WRITES.load(Ordering::Relaxed),
        fire_nanos: FIRE_NANOS.load(Ordering::Relaxed),
        max_fire_nanos: MAX_FIRE_NANOS.load(Ordering::Relaxed),
        flush_sets: FLUSH_SETS.load(Ordering::Relaxed),
        node_fires: NODE_FIRES.load(Ordering::Relaxed),
        pool_fires: POOL_FIRES.load(Ordering::Relaxed),
        max_in_flight: MAX_IN_FLIGHT.load(Ordering::Relaxed),
        walks: WALKS.load(Ordering::Relaxed),
        walk_nodes: WALK_NODES.load(Ordering::Relaxed),
        parks: PARKS.load(Ordering::Relaxed),
        side_reads: SIDE_READS.load(Ordering::Relaxed),
        undeclared_writes: UNDECLARED.load(Ordering::Relaxed),
        unbound_owned: UNBOUND_OWNED.load(Ordering::Relaxed),
        quiet_hits: QUIET_HITS.load(Ordering::Relaxed),
        validated: VALIDATED.load(Ordering::Relaxed),
        invalidated: INVALIDATED.load(Ordering::Relaxed),
        cycles: CYCLES.load(Ordering::Relaxed),
    }
}

/// Count one fanout.
pub(crate) fn record_fire() {
    FIRES.fetch_add(1, Ordering::Relaxed);
}

/// Count a read of another node's latest finished value made inside a
/// callback. The design reads it at the callback's ticket instead.
pub(crate) fn record_side_read() {
    if is_settling() {
        SIDE_READS.fetch_add(1, Ordering::Relaxed);
    }
}

// ============================================================================
// Routing a write
// ============================================================================

/// Where a write made right now goes.
pub(crate) enum Position {
    /// Into the ticket this thread is delivering.
    Joins,
    /// Into a new ticket. Keeps the target's backlog until it is seeded.
    Queue(Option<Backlogged>),
}

/// The writes to one node that became new tickets and have not been seeded.
/// While there is one, a later callback write to the node also becomes a
/// new ticket, so the node receives them in the order they were made.
#[derive(Default)]
pub(crate) struct Backlog(OnceLock<Arc<AtomicU64>>);

impl Backlog {
    fn is_empty(&self) -> bool {
        self.0
            .get()
            .is_none_or(|writes| writes.load(Ordering::Acquire) == 0)
    }

    fn add(&self) -> Backlogged {
        let writes = Arc::clone(self.0.get_or_init(Arc::default));
        writes.fetch_add(1, Ordering::AcqRel);
        Backlogged(writes)
    }
}

/// One write in a [`Backlog`]. It leaves when it is dropped.
pub(crate) struct Backlogged(Arc<AtomicU64>);

impl Drop for Backlogged {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// True when a declared edge path leads from `from` to `to`.
fn declared(from: &Arc<NodeCore>, to: &Arc<NodeCore>) -> bool {
    let key = (Arc::as_ptr(from).addr(), Arc::as_ptr(to).addr());
    let generation = EDGE_GEN.load(Ordering::Acquire);
    // Edges are only added, so a path once found stays.
    if let Some(hit) = DECLARED.with_borrow(|cache| {
        cache
            .get(&key)
            .and_then(|(seen, hit)| (*seen == generation || *hit).then_some(*hit))
    }) {
        return hit;
    }
    let mut seen: FxHashSet<usize> = FxHashSet::default();
    let mut stack = vec![Arc::clone(to)];
    let mut hit = false;
    while let Some(node) = stack.pop() {
        if Arc::ptr_eq(&node, from) {
            hit = true;
            break;
        }
        if !seen.insert(Arc::as_ptr(&node).addr()) {
            continue;
        }
        stack.extend(node.inputs.lock().iter().filter_map(Weak::upgrade));
        stack.extend(node.gated.lock().iter().filter_map(|(src, _)| src.upgrade()));
    }
    DECLARED.with_borrow_mut(|cache| {
        if cache.len() > 1 << 16 {
            cache.clear();
        }
        cache.insert(key, (generation, hit));
    });
    hit
}

/// Decide where a write to `target` made right now goes.
///
/// Outside a ticket, it is a new ticket. While seeding, it joins the ticket
/// being seeded. From a fire of node `F` at ticket `t`: a write to `F`
/// itself joins while `F` collects its value and is feedback once `F` tells
/// subscribers. A write to another node joins `t` only along a declared
/// edge from `F` (or from the node whose delivery `F` is running), and only
/// when the target has not fired or promised past `t`.
pub(crate) fn position(target: &dyn Node, backlog: &Backlog) -> Position {
    if BUILDING.with_borrow(Option::is_some) || edges::forced() {
        return Position::Queue(is_settling().then(|| backlog.add()));
    }
    let core = target.node_core();
    CTX.with_borrow(|ctx| match ctx {
        None => Position::Queue(None),
        Some(Ctx::Detached) => Position::Queue(Some(backlog.add())),
        Some(Ctx::Seeding { .. }) => Position::Joins,
        Some(Ctx::Firing(firing)) => {
            if Arc::ptr_eq(&firing.core, core) {
                return match firing.phase {
                    Phase::Collect => Position::Joins,
                    Phase::Fanout => Position::Queue(Some(backlog.add())),
                };
            }
            let along = declared(&firing.core, core)
                || firing
                    .origin
                    .as_ref()
                    .is_some_and(|origin| declared(origin, core));
            if !along {
                UNDECLARED.fetch_add(1, Ordering::Relaxed);
                // Only a write that the height rule would have joined is a
                // missing edge; a lower target was a later settle before.
                if target.node_height() > firing.stamp.height {
                    edges::undeclared(target);
                }
                return Position::Queue(Some(backlog.add()));
            }
            // A target downstream of the firing node along a declared edge
            // cannot have promised past the ticket the firing node holds.
            let joinable = core.can_join(firing.ticket);
            debug_assert!(
                joinable,
                "hyphae: promise safety: a write along a declared edge reached a node past ticket {}",
                firing.ticket
            );
            // An earlier write to the target still queued keeps the target's
            // write order: this one queues behind it.
            if backlog.is_empty() && joinable {
                return Position::Joins;
            }
            Position::Queue(Some(backlog.add()))
        }
    })
}

/// True when this thread is collecting the value of `target` for the ticket
/// it fires: a write to it is part of its own fire.
pub(crate) fn collecting(target: &dyn Node) -> bool {
    CTX.with_borrow(|ctx| match ctx {
        Some(Ctx::Firing(firing)) => {
            matches!(firing.phase, Phase::Collect) && Arc::ptr_eq(&firing.core, target.node_core())
        }
        _ => false,
    })
}

/// Push `item` to `target` at the ticket this thread is delivering. Only for
/// a write that [`position`] placed as [`Position::Joins`].
pub(crate) fn push_current(target: &Arc<dyn Node>, arrival: bool, item: Item) {
    let placed = CTX.with_borrow_mut(|ctx| match ctx {
        Some(Ctx::Seeding {
            ticket,
            index,
            touched,
        }) => {
            *index = index.saturating_add(1);
            let stamp = Stamp {
                index: *index,
                ..Stamp::SEED
            };
            touched.push(Arc::downgrade(target));
            let pushed = target
                .node_core()
                .push(*ticket, stamp, arrival, item, 0)
                .map_err(|_| *ticket);
            // A seed is the only way pending work enters an ancestry from
            // outside it. `S` passes the ticket after this.
            watermark::invalidate(target.node_core(), watermark::Cause::Seed);
            pushed
        }
        Some(Ctx::Firing(firing)) => {
            firing.stamp.index = firing.stamp.index.saturating_add(1);
            firing.touched.push(Arc::downgrade(target));
            let pushed = target
                .node_core()
                .push(
                    firing.ticket,
                    firing.stamp,
                    arrival,
                    item,
                    Arc::as_ptr(&firing.core).addr(),
                )
                .map_err(|_| firing.ticket);
            watermark::invalidate_if_marked(target.node_core());
            pushed
        }
        _ => Err(NONE),
    });
    if let Err(ticket) = placed {
        // The router never places a write that would break a promise.
        debug_assert!(
            false,
            "hyphae: promise safety: a delivery at ticket {ticket} reached a node past it"
        );
        log::error!("hyphae: a delivery at ticket {ticket} reached a node past it and was dropped");
    }
}

/// Defer a bound subscriber's callback to its owner's fire of the ticket now
/// firing. False when the owner has already passed the ticket, or this is
/// not a fanout; the caller then runs the callback now.
pub(crate) fn defer_to_owner(owner: &Arc<dyn Node>, make: impl FnOnce() -> Item) -> bool {
    CTX.with_borrow_mut(|ctx| {
        let Some(Ctx::Firing(firing)) = ctx else {
            return false;
        };
        let core = owner.node_core();
        if !matches!(firing.phase, Phase::Fanout) || Arc::ptr_eq(&firing.core, core) {
            return false;
        }
        // The owner is below the firing node along a declared edge, so it
        // cannot have passed the ticket the firing node holds.
        let joinable = core.can_join(firing.ticket);
        debug_assert!(
            joinable,
            "hyphae: promise safety: an owner passed ticket {} before its source fired it",
            firing.ticket
        );
        if !joinable {
            return false;
        }
        firing.stamp.index = firing.stamp.index.saturating_add(1);
        let origin = Arc::clone(&firing.core);
        let item = make();
        let wrapped: Item = Box::new(move || {
            let previous = CTX.with_borrow_mut(|ctx| match ctx {
                Some(Ctx::Firing(firing)) => firing.origin.replace(origin),
                _ => None,
            });
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(item));
            CTX.with_borrow_mut(|ctx| {
                if let Some(Ctx::Firing(firing)) = ctx {
                    firing.origin = previous;
                }
            });
            if let Err(payload) = outcome {
                std::panic::resume_unwind(payload);
            }
        });
        // The owner is below the firing node along a declared edge, so it
        // cannot pass this ticket while this node holds it.
        let pushed = core
            .push(
                firing.ticket,
                firing.stamp,
                false,
                wrapped,
                Arc::as_ptr(&firing.core).addr(),
            )
            .is_ok();
        watermark::invalidate_if_marked(core);
        debug_assert!(pushed, "hyphae: promise safety: owner passed the ticket");
        firing.touched.push(Arc::downgrade(owner));
        pushed
    })
}

// ============================================================================
// Readiness, parking, firing
// ============================================================================

enum Readiness {
    Idle,
    Ready,
    Blocked(Arc<NodeCore>, u64),
}

fn readiness(core: &Arc<NodeCore>) -> Readiness {
    let ticket = {
        let pending = core.pending.lock();
        match pending.items.first() {
            None => return Readiness::Idle,
            Some(first) => first.ticket,
        }
    };
    // A ticket still being seeded is not fireable: `in_w` is capped by `S`,
    // and its other seeds may not have landed. The seeder pokes every node
    // it seeded once `S` passes the ticket.
    if ticket >= SEEDED.load(Ordering::Acquire) {
        return Readiness::Idle;
    }
    if let Err(blocker) = watermark::in_w_past(core, ticket) {
        return Readiness::Blocked(blocker, ticket);
    }
    let open = core.open_gates(ticket, &core.pending.lock().items);
    for src in open {
        if let Err(blocker) = watermark::input_past(core, &src, ticket) {
            return Readiness::Blocked(blocker, ticket);
        }
    }
    Readiness::Ready
}

/// Tell `node` that it may be able to fire.
pub(crate) fn poke(node: &Arc<dyn Node>) {
    let sched = &node.node_core().sched;
    loop {
        match sched.load(Ordering::Acquire) {
            IDLE => {
                if sched
                    .compare_exchange(IDLE, CLAIMED, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    evaluate(node);
                    return;
                }
            }
            CLAIMED => {
                if sched
                    .compare_exchange(CLAIMED, DIRTY, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    return;
                }
            }
            // Confirm with a read-modify-write: a plain load can see a stale
            // dirty mark after the holder has already released.
            _ => {
                if sched
                    .compare_exchange(DIRTY, DIRTY, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    return;
                }
            }
        }
    }
}

/// With the node claimed: queue it if it is ready, otherwise park it on its
/// blocker and release the claim.
///
/// Register, then re-check. A blocker that fires before the registration
/// is seen by the re-check. One that fires after it pokes this node, which
/// marks the claim dirty and makes the release fail and loop.
fn evaluate(node: &Arc<dyn Node>) {
    let core = node.node_core();
    loop {
        // A read-modify-write, so this claim follows every poke that
        // marked it dirty.
        core.sched.swap(CLAIMED, Ordering::AcqRel);
        match readiness(core) {
            Readiness::Ready => {
                pool::ready(Arc::clone(node));
                return;
            }
            Readiness::Idle => {}
            Readiness::Blocked(blocker, ticket) => {
                PARKS.fetch_add(1, Ordering::Relaxed);
                blocker.parked.lock().push(Arc::downgrade(node));
                if blocker.pending.lock().min() > ticket {
                    continue;
                }
            }
        }
        if core
            .sched
            .compare_exchange(CLAIMED, IDLE, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            return;
        }
    }
}

/// Fire the lowest pending ticket of `node`, which this thread has claimed
/// and found ready, then evaluate it again.
pub(crate) fn run(owned: Arc<dyn Node>, on_pool: bool) {
    let node = &owned;
    let core = Arc::clone(node.node_core());
    let taken = {
        let mut pending = core.pending.lock();
        pending
            .items
            .first()
            .map(|first| first.ticket)
            .map(|ticket| {
                debug_assert!(
                    pending.processed == NONE || pending.processed < ticket,
                    "hyphae: per-node order: ticket {ticket} fired after {}",
                    pending.processed
                );
                let split = pending.items.partition_point(|d| d.ticket == ticket);
                let rest = pending.items.split_off(split);
                let items = std::mem::replace(&mut pending.items, rest);
                pending.inflight = ticket;
                (ticket, items)
            })
    };
    let Some((ticket, mut items)) = taken else {
        evaluate(node);
        return;
    };
    #[cfg(debug_assertions)]
    assert_fire_rule(&core, ticket, &items);
    items.sort_by_key(|d| d.stamp);
    let count = i64::try_from(items.len()).unwrap_or(i64::MAX);
    let started = crate::platform::Instant::now();
    let stamp = Stamp {
        height: node.node_height(),
        node: core.seq,
        index: 0,
    };
    let previous = CTX.with_borrow_mut(|ctx| {
        ctx.replace(Ctx::Firing(Firing {
            ticket,
            core: Arc::clone(&core),
            origin: None,
            stamp,
            phase: Phase::Collect,
            touched: Vec::new(),
        }))
    });
    for delivery in items {
        if let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(delivery.item))
        {
            record_panic(ticket, stamp, payload);
        }
    }
    CTX.with_borrow_mut(|ctx| {
        if let Some(Ctx::Firing(firing)) = ctx {
            firing.phase = Phase::Fanout;
        }
    });
    if let Err(payload) =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| node.emit(ticket)))
    {
        record_panic(ticket, stamp, payload);
    }
    let touched = CTX.with_borrow_mut(|ctx| match std::mem::replace(ctx, previous) {
        Some(Ctx::Firing(firing)) => firing.touched,
        _ => Vec::new(),
    });
    let nanos = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
    FIRE_NANOS.fetch_add(nanos, Ordering::Relaxed);
    MAX_FIRE_NANOS.fetch_max(nanos, Ordering::Relaxed);
    INTERVAL_MAX_FIRE_NANOS.fetch_max(nanos, Ordering::Relaxed);
    NODE_FIRES.fetch_add(1, Ordering::Relaxed);
    if on_pool {
        POOL_FIRES.fetch_add(1, Ordering::Relaxed);
    }
    let in_w = watermark::refresh_bound(&core);
    {
        let mut pending = core.pending.lock();
        pending.processed = ticket;
        pending.inflight = NONE;
        let next = pending.min();
        // Every input is past `ticket`, so `in_w` exceeds it.
        let promised = core.watermark.fetch_max(
            next.min(in_w.max(ticket.saturating_add(1))),
            Ordering::AcqRel,
        );
        debug_assert!(
            next >= promised.max(next.min(in_w.max(ticket.saturating_add(1)))),
            "hyphae: promise safety: watermark published above pending {next}"
        );
        drop(pending);
    }
    poke_all(touched);
    core.wake_parked();
    evaluate(node);
    // Release this fire's references before the ticket can complete, so a
    // writer that wakes to it sees the graph as the fire left it.
    drop(core);
    drop(owned);
    consumed(ticket, count);
}

/// The model's fire rule: no input still holds the ticket being fired.
#[cfg(debug_assertions)]
fn assert_fire_rule(core: &NodeCore, ticket: u64, items: &[Delivery]) {
    for src in core.open_gates(ticket, items) {
        let held = src.pending.lock().min();
        assert!(
            held > ticket,
            "hyphae: fire rule: ticket {ticket} fired while an open gated input holds {held}"
        );
    }
    for input in core.inputs.lock().iter().filter_map(Weak::upgrade) {
        let held = input.pending.lock().min();
        assert!(
            held > ticket,
            "hyphae: fire rule: ticket {ticket} fired while an input holds {held}"
        );
    }
}

fn poke_all(touched: Vec<Weak<dyn Node>>) {
    let mut seen: FxHashSet<usize> = FxHashSet::default();
    for node in touched {
        if seen.insert(node.as_ptr().cast::<()>().addr())
            && let Some(node) = node.upgrade()
        {
            poke(&node);
        }
    }
}

// ============================================================================
// Admission
// ============================================================================

struct EventSet {
    barrier: bool,
    epoch: Option<Epoch>,
    writes: Vec<Box<dyn Write>>,
}

impl EventSet {
    const fn empty() -> Self {
        Self {
            barrier: false,
            epoch: None,
            writes: Vec::new(),
        }
    }

    fn add_key(&mut self, key: SettleKey) {
        if key == SettleKey::Barrier {
            self.barrier = true;
        }
    }
}

/// A writer waiting for its own ticket.
struct TicketWaiter {
    ticket: AtomicU64,
}

struct Arrival {
    set: EventSet,
    waiter: Option<Arc<TicketWaiter>>,
}

static INBOX: SegQueue<Arrival> = SegQueue::new();
static ADMIT: Mutex<()> = Mutex::new(());

enum Gate {
    /// A barrier waits for every earlier ticket to complete.
    Waiting(Arrival),
    /// A seeded barrier ticket that has not completed.
    Running(u64),
}

static GATE: Mutex<Option<Gate>> = Mutex::new(None);

/// Seed what the inbox holds, in arrival order, unless another thread is
/// doing it. Never blocks: a callback that writes calls it.
pub(crate) fn admit_drain() {
    loop {
        let Some(admit) = ADMIT.try_lock() else {
            return;
        };
        let stalled = loop {
            let mut gate = GATE.lock();
            match gate.take() {
                Some(Gate::Running(ticket)) => {
                    if !is_complete(ticket) {
                        *gate = Some(Gate::Running(ticket));
                        break true;
                    }
                }
                Some(Gate::Waiting(arrival)) => {
                    if COMPLETED.load(Ordering::Acquire) < SEEDED.load(Ordering::Acquire) {
                        *gate = Some(Gate::Waiting(arrival));
                        break true;
                    }
                    *gate = Some(Gate::Running(SEEDED.load(Ordering::Acquire)));
                    drop(gate);
                    seed(arrival);
                }
                None => {
                    drop(gate);
                    if SEEDED
                        .load(Ordering::Acquire)
                        .saturating_sub(COMPLETED.load(Ordering::Acquire))
                        >= RING - 1
                    {
                        break true;
                    }
                    let Some(arrival) = INBOX.pop() else {
                        break false;
                    };
                    if arrival.set.barrier {
                        *GATE.lock() = Some(Gate::Waiting(arrival));
                    } else {
                        seed(arrival);
                    }
                }
            }
        };
        drop(admit);
        // An arrival pushed while the lock was held may have missed it. A
        // stalled drain resumes when a ticket completes.
        if stalled || INBOX.is_empty() {
            return;
        }
    }
}

/// Seed `arrival` as the next ticket. Called with the admission lock held.
fn seed(arrival: Arrival) -> u64 {
    dump::start();
    let ticket = SEEDED.load(Ordering::Acquire);
    let slot = slot(ticket);
    slot.count.store(1, Ordering::Release);
    slot.done.store(0, Ordering::Release);
    slot.epoch
        .store(arrival.set.epoch.map_or(0, Epoch::raw), Ordering::Release);
    slot.carriers.store(
        u64::from(arrival.set.epoch.is_some_and(|e| e.raw() & SCOPED != 0)),
        Ordering::Release,
    );
    *slot.panic.lock() = None;
    TICKETS.fetch_add(1, Ordering::Relaxed);
    let previous = CTX.with_borrow_mut(|ctx| {
        ctx.replace(Ctx::Seeding {
            ticket,
            index: 0,
            touched: Vec::new(),
        })
    });
    for write in arrival.set.writes {
        if let Err(payload) =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| write.apply()))
        {
            record_panic(ticket, Stamp::SEED, payload);
        }
    }
    let touched = CTX.with_borrow_mut(|ctx| match std::mem::replace(ctx, previous) {
        Some(Ctx::Seeding { touched, .. }) => touched,
        _ => Vec::new(),
    });
    SEEDED.store(ticket.saturating_add(1), Ordering::Release);
    if let Some(waiter) = &arrival.waiter {
        waiter.ticket.store(ticket, Ordering::Release);
    }
    MAX_IN_FLIGHT.fetch_max(
        ticket
            .saturating_add(1)
            .saturating_sub(COMPLETED.load(Ordering::Acquire)),
        Ordering::Relaxed,
    );
    poke_all(touched);
    consumed(ticket, 1);
    ticket
}

/// Submit one write as its own event set, or add it to the batch this
/// thread is building.
pub(crate) fn submit_write(key: SettleKey, write: Box<dyn Write>) {
    let write = BUILDING.with_borrow_mut(|building| match building.as_mut() {
        Some(building) => {
            building.set.add_key(key);
            building.set.writes.push(write);
            None
        }
        None => Some(write),
    });
    let Some(write) = write else { return };
    let mut set = EventSet::empty();
    set.writes.push(write);
    set.add_key(key);
    queue(set).finish();
}

/// An event set in the inbox whose writer has not yet waited on it.
#[must_use = "call `finish`, or the writer returns before its write has settled"]
pub(crate) struct Queued {
    mode: Mode,
    waiter: Option<Arc<TicketWaiter>>,
}

impl Queued {
    const LEFT_QUEUED: Self = Self {
        mode: Mode::Enqueue,
        waiter: None,
    };

    /// Seed the set, then wait or help as the writer's mode asks.
    pub(crate) fn finish(self) {
        if self.mode == Mode::Enqueue {
            return;
        }
        admit_drain();
        let Some(waiter) = self.waiter else { return };
        let mine = || {
            let ticket = waiter.ticket.load(Ordering::Acquire);
            ticket != NONE && is_complete(ticket)
        };
        match self.mode {
            Mode::Wait => {
                WAITS.fetch_add(1, Ordering::Relaxed);
                pool::help_until(&mine, false);
                settle_backlog();
                let ticket = waiter.ticket.load(Ordering::Acquire);
                let panic = slot(ticket).panic.lock().take().map(|(_, payload)| payload);
                if let Some(payload) = panic {
                    std::panic::resume_unwind(payload);
                }
            }
            Mode::Submit => {
                pool::help_until(&mine, true);
                settle_backlog();
            }
            Mode::Enqueue => {}
        }
    }
}

fn queue(mut set: EventSet) -> Queued {
    EVENT_SETS.fetch_add(1, Ordering::Relaxed);
    if is_settling() {
        // A callback's write: a new ticket that owes its writer nothing.
        DEFERRED_WRITES.fetch_add(1, Ordering::Relaxed);
        if set.epoch.is_none()
            && !set.writes.is_empty()
            && let Some(ticket) = current_ticket()
        {
            set.epoch = Epoch::from_raw(slot(ticket).epoch.load(Ordering::Acquire));
            carry(set.epoch);
        }
        INBOX.push(Arrival { set, waiter: None });
        pool::notify();
        return Queued::LEFT_QUEUED;
    }
    let mode = MODE.get();
    let waiter = (mode != Mode::Enqueue).then(|| {
        Arc::new(TicketWaiter {
            ticket: AtomicU64::new(NONE),
        })
    });
    INBOX.push(Arrival {
        set,
        waiter: waiter.clone(),
    });
    if mode == Mode::Enqueue {
        pool::notify();
    }
    Queued { mode, waiter }
}

// ============================================================================
// Public write API
// ============================================================================

/// Run `f` and submit every write it makes on this thread as one event set.
///
/// The writes settle together as one ticket. A read inside `f` sees the
/// value from before the batch. Nested calls join the outermost batch.
pub fn batch<R>(f: impl FnOnce() -> R) -> R {
    batch_with(&[], None, f)
}

/// [`batch`] with keys and an epoch.
pub fn batch_with<R>(keys: &[SettleKey], epoch: Option<Epoch>, f: impl FnOnce() -> R) -> R {
    let (result, set) = collect(keys, epoch, f);
    let submitted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if let Some(set) = set {
            queue(set).finish();
        }
    }));
    match (result, submitted) {
        (Ok(value), Ok(())) => value,
        (Err(payload), _) | (Ok(_), Err(payload)) => std::panic::resume_unwind(payload),
    }
}

fn collect<R>(
    keys: &[SettleKey],
    epoch: Option<Epoch>,
    f: impl FnOnce() -> R,
) -> (std::thread::Result<R>, Option<EventSet>) {
    BUILDING.with_borrow_mut(|building| {
        let building = building.get_or_insert_with(|| Building {
            depth: 0,
            set: EventSet::empty(),
        });
        building.depth = building.depth.saturating_add(1);
        for key in keys {
            building.set.add_key(*key);
        }
        if building.set.epoch.is_none() {
            building.set.epoch = epoch;
            carry(epoch);
        }
    });

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));

    let closed = BUILDING.with_borrow_mut(|building| {
        let depth = building.as_mut().map_or(0, |building| {
            building.depth = building.depth.saturating_sub(1);
            building.depth
        });
        if depth == 0 { building.take() } else { None }
    });
    let set = match closed.map(|building| building.set) {
        Some(set) if set.writes.is_empty() && !set.barrier => {
            if let Some(finished) = set.epoch.and_then(|epoch| release(epoch, 1)) {
                finished();
            }
            None
        }
        set => set,
    };
    (result, set)
}

/// [`batch_with`] that does not wait for its ticket: it seeds it and helps
/// until the ticket completes or no work is ready.
pub fn submit<R>(keys: &[SettleKey], epoch: Option<Epoch>, f: impl FnOnce() -> R) -> R {
    with_mode(Mode::Submit, || batch_with(keys, epoch, f))
}

/// [`batch_with`] that only queues. A pool thread, the next write, or
/// [`flush`] seeds it.
pub fn enqueue<R>(keys: &[SettleKey], epoch: Option<Epoch>, f: impl FnOnce() -> R) -> R {
    with_mode(Mode::Enqueue, || batch_with(keys, epoch, f))
}

fn with_mode<R>(mode: Mode, f: impl FnOnce() -> R) -> R {
    struct Restore(Mode);
    impl Drop for Restore {
        fn drop(&mut self) {
            MODE.set(self.0);
        }
    }
    let _restore = Restore(MODE.get());
    MODE.set(MODE.get().max(mode));
    f()
}

/// Wait until every ticket taken before this call has completed.
///
/// Takes a ticket with no writes and waits for the completed-through prefix
/// to pass it. Inside a callback it returns at once.
pub fn flush() {
    if is_settling() {
        return;
    }
    FLUSH_SETS.fetch_add(1, Ordering::Relaxed);
    let waiter = Arc::new(TicketWaiter {
        ticket: AtomicU64::new(NONE),
    });
    INBOX.push(Arrival {
        set: EventSet::empty(),
        waiter: Some(Arc::clone(&waiter)),
    });
    admit_drain();
    pool::help_until(
        &|| {
            let ticket = waiter.ticket.load(Ordering::Acquire);
            ticket != NONE && COMPLETED.load(Ordering::Acquire) > ticket
        },
        false,
    );
    settle_backlog();
}

/// With no pool threads, nobody else runs the tickets that callbacks queue.
/// A writer whose own ticket finished keeps firing until no work is ready
/// or queued. It does not wait for work in flight on other threads.
fn settle_backlog() {
    if pool::workers() > 0 || is_settling() {
        return;
    }
    pool::help_until(&|| false, true);
}

/// Group the writes `f` makes into one event set and queue it, for a
/// mutation that touches several cells of one collection under a lock.
///
/// The caller releases its lock and then calls [`Queued::finish`]. During a
/// ticket, and inside an open batch, `f` runs as it is and `finish` does
/// nothing.
pub(crate) fn atomic_queued<R>(f: impl FnOnce() -> R) -> (R, Queued) {
    if is_settling() || BUILDING.with_borrow(Option::is_some) {
        return (f(), Queued::LEFT_QUEUED);
    }
    let (result, set) = collect(&[], None, f);
    let queued = set.map_or(Queued::LEFT_QUEUED, queue);
    match result {
        Ok(value) => (value, queued),
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

// ============================================================================
// Heights
// ============================================================================

/// The node's height, from its cache when the cache is current.
pub(crate) fn height(node: &dyn DepNode) -> u64 {
    let mut stack = FxHashSet::default();
    height_dfs(node, &mut stack)
}

fn height_dfs(node: &dyn DepNode, stack: &mut FxHashSet<Uuid>) -> u64 {
    let epoch = node
        .height_epoch()
        .map(|epoch| u32::try_from(epoch.load(Ordering::Relaxed)).unwrap_or(u32::MAX));
    if let (Some(cache), Some(epoch)) = (node.height_cache(), epoch) {
        let packed = cache.load(Ordering::Relaxed);
        if u32::try_from(packed >> 32).unwrap_or(u32::MAX) == epoch {
            return packed & 0xFFFF_FFFF;
        }
    }
    let id = node.id();
    if !stack.insert(id) {
        return 0;
    }
    let mut height = 0u64;
    for dep in node.deps() {
        height = height.max(height_dfs(dep.as_ref(), stack).saturating_add(1));
    }
    stack.remove(&id);
    if let (Some(cache), Some(epoch)) = (node.height_cache(), epoch) {
        cache.store((u64::from(epoch) << 32) | height, Ordering::Relaxed);
    }
    height
}

/// Mark stale the cached height of `start` and of every node whose height
/// depends on it.
pub(crate) fn invalidate_height_cone(start: &dyn Node) {
    start.bump_height_epoch();
    let mut visited = FxHashSet::default();
    visited.insert(start.node_id());
    let mut stack = start.node_dependents();
    while let Some(weak) = stack.pop() {
        let Some(node) = weak.upgrade() else { continue };
        if visited.insert(node.node_id()) {
            node.bump_height_epoch();
            stack.extend(node.node_dependents());
        }
    }
}
