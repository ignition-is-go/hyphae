//! Scheduling domains: one root domain and any number of child domains, each
//! with its own propagation tick.
//!
//! # Model
//!
//! Every cell belongs to exactly one domain, fixed at birth. A cell born
//! outside any [`Domain::enter`] scope is a root cell, so a program that never
//! creates a child domain runs entirely in root with the semantics described
//! in the [scheduler module docs](super).
//!
//! Dependencies point one way: a child cell may read a root cell, and nothing
//! else crosses a domain boundary. A root cell cannot depend on a child cell,
//! and a cell cannot depend on a cell of a sibling child. Both are rejected
//! when the dependency is constructed (see [`DomainError`]).
//!
//! # The boundary
//!
//! A subscription made inside a child scope on a root cell, a root map's diff
//! stream or a root [`Source`](crate::Source) is a **hand-off** subscription.
//! When the root node fans out, the delivery is not run on the writing thread.
//! It is appended to the child's queue and the child's worker thread applies
//! it later, in root order:
//!
//! - A root write made outside a `batch` hands each delivery over as its own
//!   unit, and the child applies it eagerly. This is what a root subscriber
//!   observes for the same write.
//! - Every delivery produced by one root drain is handed over as one unit when
//!   that drain finishes, and the child applies the unit as one batch on its
//!   own tick. A child diamond fed by several root cells written in one root
//!   batch therefore settles once, as a root diamond would, and no observer in
//!   the child sees part of the unit.
//!
//! The worker finishes propagating one unit before it starts the next, so two
//! units never coalesce. Nothing is dropped and nothing is merged. Each child
//! receives each root write exactly once after the value its subscription was
//! seeded with, with one exception it shares with a subscription in root: a
//! write racing the subscription can be delivered once more after a seed that
//! already holds it. A delivery older than the seed never arrives.
//!
//! A subscription is seeded from root's current value, which can be ahead of
//! the deliveries still queued for the child's older subscriptions. Two
//! subscriptions of one child to the same root cell agree again once the
//! queue has drained ([`Domain::flush`]).
//!
//! The queue is unbounded: a child that stays slower than the root writes it
//! reads grows it without limit (see [`Domain::pending_hand_offs`]).
//!
//! # Writes
//!
//! A thread that is running a domain's propagation — a cascade, a wave, a
//! hand-off unit, a timer callback — may write only that domain's cells, maps
//! and sources. A write to another domain from there panics with
//! [`DomainError::CrossDomainWrite`] before it changes anything, because it
//! would run the other domain's cascade inside this one's wave. The only
//! crossing is explicit and points from root to a child: [`Domain::hand_off`]
//! queues a closure for the child in root order. Nothing flows from a child
//! to root or to a sibling; route such a result through code that runs
//! outside any propagation.
//!
//! A thread outside every propagation may write any domain. The write runs on
//! the written cell's tick, on the writing thread, and waits on no other
//! domain.
//!
//! # Isolation
//!
//! Each child has its own tick queue, its own single-drainer claim and its own
//! worker thread. A wave, a graph build or a graph drop that runs on one
//! child's tick holds only that child's drain. Child waves never use the
//! shared worker pool, so a wave that occupies the pool cannot hold up a
//! child, and children run in parallel on their own threads. A timer created
//! inside a child fires on that child's worker.
//!
//! # Lifetime
//!
//! Dropping the last owning [`Domain`] handle, or calling [`Domain::close`],
//! closes the child: its hand-off subscriptions are removed from the root
//! nodes, its queued units are discarded, and its worker exits.
//!
//! # wasm
//!
//! wasm has one thread, so a child domain there is logical only. It keeps its
//! own tick, batches, ordering and write rule, and has no worker: a unit is
//! applied on the calling thread, an eager one where it is handed off and a
//! batched one when the root drain that produced it finishes. A child there
//! isolates nothing.

#![allow(clippy::redundant_pub_crate)]

use std::{
    cell::RefCell,
    collections::VecDeque,
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
};

use parking_lot::{Condvar, Mutex};
use rustc_hash::FxHashMap;
use uuid::Uuid;

use super::{ReadyQueue, SharedTick};

/// The root domain's id. Child ids start at 1.
pub(crate) const ROOT_ID: u64 = 0;

/// [`PROPAGATING`] on a thread that is not running any domain's propagation.
const OUTSIDE: u64 = u64::MAX;

/// One piece of work waiting to run on a child's worker.
pub(crate) type HandOffOp = Box<dyn FnOnce() + Send>;

/// A unit of hand-off work the child applies without interleaving another unit.
enum Segment {
    /// One delivery from a root write made outside a batch, one explicit
    /// [`Domain::hand_off`], or one timer callback.
    Eager(HandOffOp),
    /// Every delivery one root drain produced for this child, in root order.
    Batch(Vec<HandOffOp>),
}

struct Inbox {
    segments: VecDeque<Segment>,
    /// Units ever queued and units fully applied. `flush` waits for the
    /// second to reach the first as it stood when the flush began.
    pushed: u64,
    applied: u64,
    closed: bool,
}

struct Mailbox {
    inbox: Mutex<Inbox>,
    arrived: Condvar,
    idle: Condvar,
    pending: AtomicUsize,
}

/// A child domain's scheduling state. The root domain has no `DomainInner`:
/// it is `None` wherever a domain is optional, and its tick is the scheduler's
/// process-wide static.
pub(crate) struct DomainInner {
    pub(crate) id: u64,
    pub(super) tick: Mutex<SharedTick>,
    pub(super) active: AtomicBool,
    /// Signalled when this tick's drain is released, so a thread waiting for
    /// the tick to go quiet can look again.
    pub(super) released: Condvar,
    mailbox: Arc<Mailbox>,
    closed: AtomicBool,
    /// How to remove each hand-off subscription this domain holds on a root
    /// cell or source, by subscription id. Closing the domain runs them all.
    boundary: Mutex<FxHashMap<Uuid, HandOffOp>>,
    #[cfg(not(target_arch = "wasm32"))]
    worker: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl DomainInner {
    pub(super) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// Remember how to remove hand-off subscription `id`. A domain that is
    /// already closed removes it at once.
    pub(crate) fn register_boundary(&self, id: Uuid, unsubscribe: HandOffOp) {
        let mut boundary = self.boundary.lock();
        if self.is_closed() {
            drop(boundary);
            unsubscribe();
            return;
        }
        boundary.insert(id, unsubscribe);
    }

    /// Forget hand-off subscription `id`, which its guard just removed.
    pub(crate) fn release_boundary(&self, id: Uuid) {
        let released = self.boundary.lock().remove(&id);
        drop(released);
    }

    /// Stop the domain: remove its hand-off subscriptions, discard what is
    /// queued for it, and stop its worker. Blocks until the worker has exited
    /// unless the caller is that worker.
    fn close(&self) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        let boundary = std::mem::take(&mut *self.boundary.lock());
        for (_id, unsubscribe) in boundary {
            unsubscribe();
        }
        let discarded = {
            let mut inbox = self.mailbox.inbox.lock();
            inbox.closed = true;
            std::mem::take(&mut inbox.segments)
        };
        self.mailbox.pending.store(0, Ordering::Relaxed);
        self.mailbox.arrived.notify_all();
        self.mailbox.idle.notify_all();
        drop(discarded);
        // The worker may be waiting for this tick to go quiet. Taking the
        // lock orders this wake after its check of `closed`.
        drop(self.tick.lock());
        self.released.notify_all();
        #[cfg(not(target_arch = "wasm32"))]
        {
            let worker = self.worker.lock().take();
            if let Some(worker) = worker
                && worker.thread().id() != std::thread::current().id()
                && worker.join().is_err()
            {
                log::error!("hyphae: the worker of domain {} panicked", self.id);
            }
        }
    }
}

/// Set once the first child domain exists. Until then every domain check on a
/// hot path is this one relaxed load.
static DOMAINS_IN_USE: AtomicBool = AtomicBool::new(false);
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Deliveries the root tick has produced for child domains and not yet
/// distributed, in root order. A root drain fills it and distributes it when
/// the drain runs dry. Every hand-off takes this lock, which is what keeps
/// deliveries to one child in the order root produced them.
static ROOT_OUTBOX: Mutex<Vec<(Arc<DomainInner>, HandOffOp)>> = Mutex::new(Vec::new());

thread_local! {
    /// The domain cells born on this thread join, and the one a bare
    /// [`batch`](super::batch) opens. `None` is root.
    static AMBIENT: RefCell<Option<Arc<DomainInner>>> = const { RefCell::new(None) };
    /// Id of the domain whose propagation this thread is running: a cascade,
    /// a wave, a hand-off unit or a timer callback. [`OUTSIDE`] otherwise.
    static PROPAGATING: std::cell::Cell<u64> = const { std::cell::Cell::new(OUTSIDE) };
    /// Whether this thread is running an op of a root drain. A hand-off made
    /// then belongs to that drain's unit.
    static ROOT_WAVE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[inline]
pub(crate) fn in_use() -> bool {
    DOMAINS_IN_USE.load(Ordering::Relaxed)
}

fn ambient() -> Option<Arc<DomainInner>> {
    AMBIENT
        .try_with(|ambient| ambient.borrow().clone())
        .unwrap_or(None)
}

/// The child domain a bare `batch` on this thread opens, if any.
#[inline]
pub(crate) fn ambient_child() -> Option<Arc<DomainInner>> {
    if in_use() { ambient() } else { None }
}

/// The domain a cell born on this thread joins.
#[inline]
pub(crate) fn birth_domain() -> Option<Arc<DomainInner>> {
    ambient_child()
}

/// The ambient domain's id, [`ROOT_ID`] when no child scope is active.
#[inline]
pub(crate) fn ambient_id() -> u64 {
    if !in_use() {
        return ROOT_ID;
    }
    AMBIENT
        .try_with(|ambient| {
            ambient
                .borrow()
                .as_ref()
                .map_or(ROOT_ID, |domain| domain.id)
        })
        .unwrap_or(ROOT_ID)
}

/// Whether this thread is working inside a child domain.
#[inline]
pub(crate) fn in_child_domain() -> bool {
    ambient_id() != ROOT_ID
}

/// The child a subscription made on this thread hands off to, given the
/// subscribed node's domain. Only a root node read from a child scope crosses
/// the boundary.
#[inline]
pub(crate) fn hand_off_target(source: Option<&Arc<DomainInner>>) -> Option<Arc<DomainInner>> {
    if source.is_some() {
        return None;
    }
    ambient_child()
}

pub(crate) fn id_of(domain: Option<&Arc<DomainInner>>) -> u64 {
    domain.map_or(ROOT_ID, |domain| domain.id)
}

/// Whether this thread may write a cell, map or source of domain `target`
/// right now. A thread running one domain's propagation writes only that
/// domain: a write to another would run the other domain's cascade inside
/// this one's wave.
///
/// # Errors
///
/// Returns [`DomainError::CrossDomainWrite`] for a write from inside another
/// domain's propagation.
#[inline]
pub(crate) fn check_write(target: u64) -> Result<(), DomainError> {
    let from = PROPAGATING
        .try_with(std::cell::Cell::get)
        .unwrap_or(OUTSIDE);
    if from == OUTSIDE || from == target {
        Ok(())
    } else {
        Err(DomainError::CrossDomainWrite)
    }
}

/// [`check_write`], free until the first child domain exists, panicking on a
/// rejected write.
#[inline]
pub(crate) fn assert_write(target: Option<&Arc<DomainInner>>) {
    if in_use()
        && let Err(error) = check_write(id_of(target))
    {
        reject(error);
    }
}

/// A dependency or a write that crosses a scheduling-domain boundary the wrong
/// way is an error in the calling code, reported where it happens.
#[allow(clippy::panic)]
pub(crate) fn reject(error: DomainError) -> ! {
    panic!("hyphae: {error}");
}

/// Restores the previous ambient domain when dropped.
pub(crate) struct AmbientScope {
    previous: Previous,
}

enum Previous {
    /// The scope entered the domain that was already ambient.
    Unchanged,
    Was(Option<Arc<DomainInner>>),
}

impl Drop for AmbientScope {
    fn drop(&mut self) {
        if let Previous::Was(previous) = std::mem::replace(&mut self.previous, Previous::Unchanged)
        {
            let displaced = AMBIENT
                .try_with(|ambient| std::mem::replace(&mut *ambient.borrow_mut(), previous))
                .ok();
            drop(displaced);
        }
    }
}

/// Make `domain` the ambient domain of this thread until the scope drops.
pub(crate) fn enter(domain: Option<&Arc<DomainInner>>) -> AmbientScope {
    let previous = AMBIENT
        .try_with(|ambient| {
            let mut ambient = ambient.borrow_mut();
            let unchanged = match (ambient.as_ref(), domain) {
                (None, None) => true,
                (Some(current), Some(next)) => Arc::ptr_eq(current, next),
                _ => false,
            };
            if unchanged {
                Previous::Unchanged
            } else {
                Previous::Was(std::mem::replace(&mut *ambient, domain.cloned()))
            }
        })
        .unwrap_or(Previous::Unchanged);
    AmbientScope { previous }
}

/// Marks this thread as running `domain`'s propagation until dropped, with
/// `domain` as its ambient domain.
pub(crate) struct PropagationScope {
    _ambient: AmbientScope,
    previous: u64,
}

impl Drop for PropagationScope {
    fn drop(&mut self) {
        let _ = PROPAGATING.try_with(|propagating| propagating.set(self.previous));
    }
}

pub(crate) fn propagate(domain: Option<&Arc<DomainInner>>) -> PropagationScope {
    let previous = PROPAGATING
        .try_with(|propagating| propagating.replace(id_of(domain)))
        .unwrap_or(OUTSIDE);
    PropagationScope {
        _ambient: enter(domain),
        previous,
    }
}

/// [`propagate`], skipped entirely while no child domain exists.
#[inline]
pub(crate) fn propagate_if_in_use(domain: Option<&Arc<DomainInner>>) -> Option<PropagationScope> {
    in_use().then(|| propagate(domain))
}

/// Marks this thread as running a root drain op until dropped.
pub(super) struct RootWaveScope {
    previous: bool,
}

impl Drop for RootWaveScope {
    fn drop(&mut self) {
        let _ = ROOT_WAVE.try_with(|flag| flag.set(self.previous));
    }
}

pub(super) fn enter_root_wave() -> RootWaveScope {
    let previous = ROOT_WAVE
        .try_with(|flag| flag.replace(true))
        .unwrap_or(false);
    RootWaveScope { previous }
}

/// Hand `op` across the boundary to `target`, in root order. Called by a root
/// cell's fanout for each hand-off subscriber, and by [`Domain::hand_off`].
///
/// A hand-off made by a root drain waits in the outbox for that drain to
/// finish. So does one made while the outbox holds anything, because pushing
/// it straight to the child would overtake the deliveries already waiting.
pub(crate) fn hand_off(target: &Arc<DomainInner>, op: HandOffOp) {
    let mut outbox = ROOT_OUTBOX.lock();
    if !outbox.is_empty() || ROOT_WAVE.try_with(std::cell::Cell::get).unwrap_or(false) {
        outbox.push((Arc::clone(target), op));
        return;
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        // Pushed under the outbox lock, so a drain that starts filling the
        // outbox now cannot distribute ahead of this unit.
        let rejected = push(target, Segment::Eager(op));
        drop(outbox);
        drop(rejected);
    }
    #[cfg(target_arch = "wasm32")]
    {
        drop(outbox);
        apply(target, Segment::Eager(op));
    }
}

/// What a finished root drain handed over. [`finish`](Self::finish) must run
/// after the root tick is unlocked.
#[must_use = "call `finish` after releasing the root tick"]
pub(super) struct HandedOff {
    /// Domain handles and refused units. Dropping a last handle, or a unit,
    /// can drop cells, which must not happen under the root tick.
    released: Vec<Arc<DomainInner>>,
    refused: Vec<Segment>,
    /// wasm has no worker: the units are applied by the thread that drained.
    #[cfg(target_arch = "wasm32")]
    units: Vec<(Arc<DomainInner>, Segment)>,
}

impl HandedOff {
    pub(super) fn finish(self) {
        drop(self.refused);
        #[cfg(target_arch = "wasm32")]
        for (target, unit) in self.units {
            apply(&target, unit);
        }
        drop(self.released);
    }
}

/// Distribute the finished root drain's deliveries, one unit per child. Called
/// with the root tick locked so two drains' units reach each child in drain
/// order.
pub(super) fn flush_root_outbox() -> HandedOff {
    let mut outbox = ROOT_OUTBOX.lock();
    let entries = std::mem::take(&mut *outbox);
    let mut index: FxHashMap<u64, usize> = FxHashMap::default();
    let mut units: Vec<(Arc<DomainInner>, Vec<HandOffOp>)> = Vec::new();
    for (target, op) in entries {
        let slot = *index.entry(target.id).or_insert(units.len());
        if let Some((_, ops)) = units.get_mut(slot) {
            ops.push(op);
        } else {
            units.push((target, vec![op]));
        }
    }
    let mut handed_off = HandedOff {
        released: Vec::with_capacity(units.len()),
        refused: Vec::new(),
        #[cfg(target_arch = "wasm32")]
        units: Vec::new(),
    };
    for (target, ops) in units {
        #[cfg(not(target_arch = "wasm32"))]
        handed_off
            .refused
            .extend(push(&target, Segment::Batch(ops)));
        #[cfg(target_arch = "wasm32")]
        handed_off
            .units
            .push((Arc::clone(&target), Segment::Batch(ops)));
        handed_off.released.push(target);
    }
    drop(outbox);
    handed_off
}

/// Queue `segment` for `target`'s worker. A closed domain refuses it and gives
/// it back for the caller to drop once it holds no lock.
#[cfg(not(target_arch = "wasm32"))]
#[must_use]
fn push(target: &Arc<DomainInner>, segment: Segment) -> Option<Segment> {
    let mailbox = &target.mailbox;
    let mut inbox = mailbox.inbox.lock();
    if inbox.closed {
        return Some(segment);
    }
    inbox.segments.push_back(segment);
    inbox.pushed = inbox.pushed.saturating_add(1);
    mailbox.pending.fetch_add(1, Ordering::Relaxed);
    drop(inbox);
    mailbox.arrived.notify_one();
    None
}

/// The domain that owns a timer registered on this thread now, when it is a
/// child. Such a timer's callback runs on that child's worker.
pub(crate) fn timer_owner() -> Option<Weak<DomainInner>> {
    ambient_child().map(|owner| Arc::downgrade(&owner))
}

/// Run `task` on `owner`'s tick: on its worker thread, as a unit of its own.
/// Returns `false` when the domain is closed and the task was dropped.
pub(crate) fn run_task(owner: &Arc<DomainInner>, task: HandOffOp) -> bool {
    #[cfg(not(target_arch = "wasm32"))]
    {
        push(owner, Segment::Eager(task)).is_none()
    }
    #[cfg(target_arch = "wasm32")]
    {
        if owner.is_closed() {
            return false;
        }
        apply(owner, Segment::Eager(task));
        true
    }
}

pub(super) fn log_panic(context: &str, payload: &(dyn std::any::Any + Send)) {
    let message = payload
        .downcast_ref::<&str>()
        .map(|message| (*message).to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "non-string panic payload".to_string());
    log::error!("hyphae: {context} panicked: {message}");
}

fn run_logged(op: HandOffOp) {
    if let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(op)) {
        log_panic("hand-off delivery", payload.as_ref());
    }
}

fn apply(domain: &Arc<DomainInner>, segment: Segment) {
    let _scope = propagate(Some(domain));
    match segment {
        Segment::Eager(op) => run_logged(op),
        Segment::Batch(ops) => {
            let drained = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                super::batch_in(Some(domain), || {
                    for op in ops {
                        run_logged(op);
                    }
                });
            }));
            if let Err(payload) = drained {
                log_panic("hand-off batch drain", payload.as_ref());
            }
        }
    }
}

/// The next unit to apply, or `None` once the domain is closed.
#[cfg(not(target_arch = "wasm32"))]
fn next_segment(mailbox: &Mailbox) -> Option<Segment> {
    let mut inbox = mailbox.inbox.lock();
    loop {
        if inbox.closed {
            drop(inbox);
            return None;
        }
        if let Some(segment) = inbox.segments.pop_front() {
            drop(inbox);
            return Some(segment);
        }
        mailbox.arrived.wait(&mut inbox);
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn run_worker(domain: &Weak<DomainInner>, mailbox: &Mailbox) {
    while let Some(segment) = next_segment(mailbox) {
        match domain.upgrade() {
            Some(domain) => {
                apply(&domain, segment);
                // The unit is fully propagated before the next one starts, so
                // two units never coalesce.
                super::quiesce(Some(&domain));
            }
            None => drop(segment),
        }
        let mut inbox = mailbox.inbox.lock();
        inbox.applied = inbox.applied.saturating_add(1);
        if !inbox.closed {
            mailbox.pending.fetch_sub(1, Ordering::Relaxed);
        }
        drop(inbox);
        mailbox.idle.notify_all();
    }
}

/// Closes the domain when the last owning [`Domain`] handle drops.
struct Owner(Arc<DomainInner>);

impl Drop for Owner {
    fn drop(&mut self) {
        self.0.close();
    }
}

/// A scheduling domain. Cheap to clone; clones name the same domain.
///
/// The handle [`child`](Self::child) returns, and its clones, own the domain:
/// dropping the last of them closes it (see [`close`](Self::close)). A handle
/// obtained from [`current`](Self::current) or
/// [`Cell::domain`](crate::Cell::domain) names the domain without owning it.
///
/// See the [module docs](crate::scheduler::domain) for the model.
#[derive(Clone)]
pub struct Domain {
    inner: Option<Arc<DomainInner>>,
    /// Held only for its drop, which closes the domain.
    _owner: Option<Arc<Owner>>,
}

impl Domain {
    /// The root domain, where every cell lives unless it is born inside a
    /// child's [`enter`](Self::enter) scope.
    #[must_use]
    pub const fn root() -> Self {
        Self {
            inner: None,
            _owner: None,
        }
    }

    /// Create a child domain with its own tick and worker thread.
    #[must_use]
    pub fn child() -> Self {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let mailbox = Arc::new(Mailbox {
            inbox: Mutex::new(Inbox {
                segments: VecDeque::new(),
                pushed: 0,
                applied: 0,
                closed: false,
            }),
            arrived: Condvar::new(),
            idle: Condvar::new(),
            pending: AtomicUsize::new(0),
        });
        let inner = Arc::new(DomainInner {
            id,
            tick: Mutex::new(SharedTick {
                ready: ReadyQueue::new(),
                depth: 0,
                draining: false,
            }),
            active: AtomicBool::new(false),
            released: Condvar::new(),
            mailbox: Arc::clone(&mailbox),
            closed: AtomicBool::new(false),
            boundary: Mutex::new(FxHashMap::default()),
            #[cfg(not(target_arch = "wasm32"))]
            worker: Mutex::new(None),
        });
        DOMAINS_IN_USE.store(true, Ordering::Relaxed);
        #[cfg(not(target_arch = "wasm32"))]
        {
            let weak = Arc::downgrade(&inner);
            let spawned = std::thread::Builder::new()
                .name(format!("hyphae-domain-{id}"))
                .spawn(move || run_worker(&weak, &mailbox));
            match spawned {
                Ok(worker) => *inner.worker.lock() = Some(worker),
                Err(error) => log::error!(
                    "hyphae: could not start the worker of domain {id}, so nothing handed to \
                     it will run: {error}"
                ),
            }
        }
        Self {
            _owner: Some(Arc::new(Owner(Arc::clone(&inner)))),
            inner: Some(inner),
        }
    }

    /// The domain cells born on the calling thread currently join. The handle
    /// does not own the domain.
    #[must_use]
    pub fn current() -> Self {
        Self::from_inner(ambient_child())
    }

    pub(crate) const fn from_inner(inner: Option<Arc<DomainInner>>) -> Self {
        Self {
            inner,
            _owner: None,
        }
    }

    #[must_use]
    pub const fn is_root(&self) -> bool {
        self.inner.is_none()
    }

    /// Whether this child domain has been closed. Always `false` for root.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.inner.as_ref().is_some_and(|domain| domain.is_closed())
    }

    /// Run `f` with this domain as the calling thread's ambient domain.
    ///
    /// Every cell, map and source born while `f` runs belongs to this domain,
    /// and every subscription `f` makes on a root cell or source is a hand-off
    /// subscription. Build a child's graph inside this scope. Cells that the
    /// child's own operators create later, while one of its waves runs, join
    /// it without a scope.
    pub fn enter<R>(&self, f: impl FnOnce() -> R) -> R {
        let _scope = enter(self.inner.as_ref());
        f()
    }

    /// Run `f` as one propagation batch on this domain's tick, inside an
    /// [`enter`](Self::enter) scope.
    ///
    /// The batch is atomic to observers in this domain: the writes `f` makes
    /// to this domain's cells reach its tick together when `f` returns, so no
    /// observer sees some of them without the rest, whichever thread drains.
    /// A write `f` makes to a cell of another domain is not part of the batch
    /// and takes effect on that domain's tick at once.
    pub fn batch<R>(&self, f: impl FnOnce() -> R) -> R {
        let _scope = enter(self.inner.as_ref());
        super::batch_in(self.inner.as_ref(), f)
    }

    /// Queue `f` to run on this child domain's tick, on its worker thread, and
    /// return without waiting for it. `f` runs in root order: after every root
    /// write handed to this domain before the call, and before every one
    /// handed over after it. Called from a root batch's propagation, `f`
    /// belongs to the unit that batch hands over.
    ///
    /// This is the one way to act on a child from inside root's propagation,
    /// and the way to route work to a child without running it on the calling
    /// thread. `f` is dropped unrun if the domain is closed.
    ///
    /// On the root domain `f` runs at once on the calling thread.
    ///
    /// # Panics
    ///
    /// Panics if the calling thread is running the propagation of a child
    /// domain other than this one: nothing flows from a child to root or to a
    /// sibling.
    pub fn hand_off(&self, f: impl FnOnce() + Send + 'static) {
        if in_use()
            && let Err(error) = check_origin(id_of(self.inner.as_ref()))
        {
            reject(error);
        }
        match self.inner.as_ref() {
            None => f(),
            Some(domain) => hand_off(domain, Box::new(f)),
        }
    }

    /// Number of units queued for this domain's worker and not yet fully
    /// applied: hand-off units, explicit hand-offs and timer callbacks. Always
    /// zero for root.
    ///
    /// The queue is unbounded by design. Bounding it would mean blocking the
    /// root writer on a slow child, or merging units, and the domain model
    /// forbids both. A child that stays slower than the root writes it reads
    /// therefore grows this without limit, and the memory with it: watch it,
    /// and slow the root writer down or close the child when it climbs.
    #[must_use]
    pub fn pending_hand_offs(&self) -> usize {
        self.inner
            .as_ref()
            .map_or(0, |domain| domain.mailbox.pending.load(Ordering::Relaxed))
    }

    /// Block until this domain has applied and propagated every root write
    /// that returned before the call, and everything else queued for its
    /// worker by then. Returns at once for root and for a closed domain.
    ///
    /// The calling thread first waits for root's propagation to finish, and
    /// runs it itself if no other thread is.
    ///
    /// # Panics
    ///
    /// Panics if the calling thread is running any domain's propagation: the
    /// propagation it would wait for is its own.
    pub fn flush(&self) {
        let Some(domain) = self.inner.as_ref() else {
            return;
        };
        if PROPAGATING
            .try_with(std::cell::Cell::get)
            .is_ok_and(|from| from != OUTSIDE)
        {
            reject(DomainError::WaitInsidePropagation);
        }
        super::quiesce(None);
        let mailbox = &domain.mailbox;
        let mut inbox = mailbox.inbox.lock();
        let target = inbox.pushed;
        while inbox.applied < target && !inbox.closed {
            mailbox.idle.wait(&mut inbox);
        }
    }

    /// Close this child domain. Does nothing for root or a closed domain.
    ///
    /// Closing removes every hand-off subscription the domain holds on root
    /// cells and sources, discards the units still queued for it, and stops
    /// its worker thread. The call returns once the worker has exited, after
    /// the unit it was applying, unless the caller is the worker itself.
    /// Nothing is delivered to the domain afterwards: later root writes skip
    /// it, and its timers stop.
    ///
    /// The domain's cells stay valid. A write to one still propagates inside
    /// the domain on the writing thread, and each is freed when its last
    /// handle drops. Dropping the last owning handle closes the domain too.
    pub fn close(&self) {
        if let Some(domain) = self.inner.as_ref() {
            domain.close();
        }
    }
}

/// Whether this thread may hand work to domain `target`: from outside any
/// propagation, from root's, or from `target`'s own.
fn check_origin(target: u64) -> Result<(), DomainError> {
    let from = PROPAGATING
        .try_with(std::cell::Cell::get)
        .unwrap_or(OUTSIDE);
    if from == OUTSIDE || from == ROOT_ID || from == target {
        Ok(())
    } else {
        Err(DomainError::CrossDomainWrite)
    }
}

impl PartialEq for Domain {
    fn eq(&self, other: &Self) -> bool {
        id_of(self.inner.as_ref()) == id_of(other.inner.as_ref())
    }
}

impl Eq for Domain {}

impl std::fmt::Debug for Domain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.inner.as_ref() {
            None => f.write_str("Domain(root)"),
            Some(domain) => write!(f, "Domain(child {})", domain.id),
        }
    }
}

/// A dependency that would cross a domain boundary the wrong way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DomainError {
    /// A root cell would depend on a cell of a child domain.
    RootReadsChild,
    /// A cell of one child domain would depend on a cell of another.
    SiblingRead,
    /// A child cell would own a subscription to a root cell that was made
    /// outside that child's scope, so it is not a hand-off subscription.
    UnscopedBoundary,
    /// A thread running one domain's propagation wrote a cell, map or source
    /// of another domain, or handed work from a child to root or a sibling.
    CrossDomainWrite,
    /// A thread running a propagation called a function that waits for
    /// propagation to finish.
    WaitInsidePropagation,
}

impl std::fmt::Display for DomainError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::RootReadsChild => {
                "a root-domain cell cannot depend on a child-domain cell; build cells that read a \
                 child inside that child's `Domain::enter` scope"
            }
            Self::SiblingRead => {
                "a cell cannot depend on a cell of a sibling child domain; share the data through \
                 the root domain"
            }
            Self::UnscopedBoundary => {
                "a child-domain cell cannot own a root-cell subscription made outside its domain; \
                 subscribe inside that child's `Domain::enter` scope"
            }
            Self::CrossDomainWrite => {
                "a write from inside one scheduling domain's propagation cannot target another \
                 domain; from root use `Domain::hand_off`, and route a child's result through \
                 code that runs outside any propagation"
            }
            Self::WaitInsidePropagation => {
                "`Domain::flush` cannot be called from inside a propagation, which it would wait \
                 for"
            }
        })
    }
}

impl std::error::Error for DomainError {}

/// Check a dependency of a cell in `reader` on a cell in `source`. `Ok(true)`
/// means the edge crosses the boundary from root into a child.
pub(crate) const fn check_edge(reader: u64, source: u64) -> Result<bool, DomainError> {
    if source == reader {
        Ok(false)
    } else if source == ROOT_ID {
        Ok(true)
    } else if reader == ROOT_ID {
        Err(DomainError::RootReadsChild)
    } else {
        Err(DomainError::SiblingRead)
    }
}
