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
//! A subscription made inside a child scope on a root cell is a **hand-off**
//! subscription. When the root cell fans out, the delivery is not run on the
//! writing thread. It is appended to the child's hand-off queue and the child's
//! worker thread applies it later, in root order:
//!
//! - A root write made outside a `batch` hands each delivery over as its own
//!   unit, and the child applies it eagerly. This is what a root subscriber
//!   observes for the same write.
//! - Every delivery produced by one root drain is handed over as one unit when
//!   that drain finishes, and the child applies the unit as one batch on its own
//!   tick. A child diamond fed by several root cells written in one root batch
//!   therefore settles once, as a root diamond would.
//!
//! The worker waits for the child's tick to go quiet between units, so two
//! units never coalesce into one. Nothing is dropped. The queue is unbounded:
//! a child that stays slower than root writes grows it without limit (see
//! [`Domain::pending_hand_offs`]).
//!
//! # Isolation
//!
//! Each child has its own tick queue, its own single-drainer claim and its own
//! worker thread. A wave, a graph build or a graph drop that runs on one
//! child's tick holds only that child's drain. Child waves never use the
//! shared worker pool, so a wave that occupies the pool cannot hold up a
//! child, and children run in parallel on their own threads.

#![allow(clippy::redundant_pub_crate)]

use std::{
    cell::RefCell,
    collections::VecDeque,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
};

use parking_lot::{Condvar, Mutex};
use rustc_hash::FxHashMap;

use super::{ReadyQueue, SharedTick};

/// The root domain's id. Child ids start at 1.
pub(crate) const ROOT_ID: u64 = 0;

/// One delivery waiting to cross the boundary into a child.
pub(crate) type HandOffOp = Box<dyn FnOnce() + Send>;

/// A unit of hand-off work the child applies without interleaving another unit.
enum Segment {
    /// One delivery from a root write made outside a batch.
    Eager(HandOffOp),
    /// Every delivery one root drain produced for this child, in root order.
    Batch(Vec<HandOffOp>),
}

struct Inbox {
    segments: VecDeque<Segment>,
    applying: bool,
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
    /// Signalled when this tick's drain is released or its last open batch
    /// closes, so the hand-off worker can wait for the tick to go quiet.
    pub(super) released: Condvar,
    mailbox: Arc<Mailbox>,
}

impl Drop for DomainInner {
    fn drop(&mut self) {
        self.mailbox.inbox.lock().closed = true;
        self.mailbox.arrived.notify_all();
    }
}

/// Set once the first child domain exists. Until then every domain check on a
/// hot path is this one relaxed load.
static DOMAINS_IN_USE: AtomicBool = AtomicBool::new(false);
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Deliveries produced by the root drain in flight, in root order. Distributed
/// to the children when that drain finishes.
static ROOT_OUTBOX: Mutex<Vec<(Arc<DomainInner>, HandOffOp)>> = Mutex::new(Vec::new());
static ROOT_OUTBOX_PENDING: AtomicBool = AtomicBool::new(false);

thread_local! {
    /// The domain cells born on this thread join, and the one a bare
    /// [`batch`](super::batch) opens. `None` is root.
    static AMBIENT: RefCell<Option<Arc<DomainInner>>> = const { RefCell::new(None) };
    /// Whether this thread is running an op of a root drain. A hand-off made
    /// then belongs to that drain's unit.
    static ROOT_WAVE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// This thread's open-batch nesting per child domain.
    static CHILD_NEST: RefCell<Vec<(u64, u32)>> = const { RefCell::new(Vec::new()) };
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
pub(super) fn ambient_child() -> Option<Arc<DomainInner>> {
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
/// subscribed cell's domain. Only a root cell read from a child scope crosses
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

/// [`enter`], skipped entirely while no child domain exists.
#[inline]
pub(crate) fn enter_if_in_use(domain: Option<&Arc<DomainInner>>) -> Option<AmbientScope> {
    in_use().then(|| enter(domain))
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

/// Record one more open batch of child `id` on this thread. Returns whether it
/// is this thread's outermost batch of that domain.
pub(super) fn nest_enter(id: u64) -> bool {
    CHILD_NEST.with(|nest| {
        let mut nest = nest.borrow_mut();
        if let Some((_, depth)) = nest.iter_mut().find(|(held, _)| *held == id) {
            *depth = depth.saturating_add(1);
            false
        } else {
            nest.push((id, 1));
            true
        }
    })
}

pub(super) fn nest_exit(id: u64) {
    CHILD_NEST.with(|nest| {
        let mut nest = nest.borrow_mut();
        if let Some(index) = nest.iter().position(|(held, _)| *held == id) {
            let finished = nest.get_mut(index).is_some_and(|(_, depth)| {
                *depth = depth.saturating_sub(1);
                *depth == 0
            });
            if finished {
                nest.swap_remove(index);
            }
        }
    });
}

/// Hand `op` across the boundary to `target`. Called by a root cell's fanout
/// for each hand-off subscriber.
pub(crate) fn hand_off(target: &Arc<DomainInner>, op: HandOffOp) {
    if ROOT_WAVE.try_with(std::cell::Cell::get).unwrap_or(false) {
        ROOT_OUTBOX.lock().push((Arc::clone(target), op));
        ROOT_OUTBOX_PENDING.store(true, Ordering::Release);
    } else {
        push(target, Segment::Eager(op));
    }
}

/// Distribute the finished root drain's deliveries, one unit per child. Called
/// with the root tick locked so two drains' units reach each child in drain
/// order. The returned domain handles must be dropped after that lock is
/// released: a last handle drops the child's queued work.
#[must_use = "drop the returned handles after releasing the root tick"]
pub(super) fn flush_root_outbox() -> Vec<Arc<DomainInner>> {
    if !ROOT_OUTBOX_PENDING.swap(false, Ordering::AcqRel) {
        return Vec::new();
    }
    let entries = std::mem::take(&mut *ROOT_OUTBOX.lock());
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
    units
        .into_iter()
        .map(|(target, ops)| {
            push(&target, Segment::Batch(ops));
            target
        })
        .collect()
}

#[cfg(not(target_arch = "wasm32"))]
fn push(target: &Arc<DomainInner>, segment: Segment) {
    let mailbox = &target.mailbox;
    let mut inbox = mailbox.inbox.lock();
    inbox.segments.push_back(segment);
    mailbox.pending.fetch_add(1, Ordering::Relaxed);
    drop(inbox);
    mailbox.arrived.notify_one();
}

/// wasm has one thread, so there is nothing to isolate and no worker to run:
/// the unit is applied where it is handed off.
#[cfg(target_arch = "wasm32")]
fn push(target: &Arc<DomainInner>, segment: Segment) {
    apply(target, segment);
}

fn log_panic(context: &str, payload: &(dyn std::any::Any + Send)) {
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
    match segment {
        Segment::Eager(op) => {
            let _scope = enter(Some(domain));
            run_logged(op);
        }
        Segment::Batch(ops) => {
            let _scope = enter(Some(domain));
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

/// Block until `domain`'s tick has no open batch, no drain in flight and no
/// queued op. The unit just applied has then been fully propagated, so the
/// next unit cannot coalesce with it.
#[cfg(not(target_arch = "wasm32"))]
fn settle(domain: &Arc<DomainInner>) {
    loop {
        let mut tick = domain.tick.lock();
        if tick.draining || tick.depth > 0 {
            domain.released.wait(&mut tick);
            continue;
        }
        if tick.ready.is_empty() {
            return;
        }
        tick.draining = true;
        super::refresh_active(&domain.active, &tick);
        drop(tick);
        if let Some(payload) = super::drain(Some(domain)) {
            log_panic("hand-off settle drain", payload.as_ref());
        }
    }
}

/// The next unit to apply, or `None` once the domain is gone and its queue is
/// empty.
#[cfg(not(target_arch = "wasm32"))]
fn next_segment(mailbox: &Mailbox) -> Option<Segment> {
    let mut inbox = mailbox.inbox.lock();
    loop {
        if let Some(segment) = inbox.segments.pop_front() {
            inbox.applying = true;
            drop(inbox);
            return Some(segment);
        }
        if inbox.closed {
            drop(inbox);
            return None;
        }
        mailbox.arrived.wait(&mut inbox);
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn run_worker(domain: &std::sync::Weak<DomainInner>, mailbox: &Mailbox) {
    while let Some(segment) = next_segment(mailbox) {
        // A queued delivery holds its subscriber, and a hand-off subscriber
        // holds its domain, so the domain is alive while a unit is queued.
        if let Some(domain) = domain.upgrade() {
            apply(&domain, segment);
            settle(&domain);
        }
        mailbox.pending.fetch_sub(1, Ordering::Relaxed);
        let mut inbox = mailbox.inbox.lock();
        inbox.applying = false;
        let idle = inbox.segments.is_empty();
        drop(inbox);
        if idle {
            mailbox.idle.notify_all();
        }
    }
}

/// A scheduling domain. Cheap to clone; clones name the same domain.
///
/// See the [module docs](self) for the model.
#[derive(Clone)]
pub struct Domain(Option<Arc<DomainInner>>);

impl Domain {
    /// The root domain, where every cell lives unless it is born inside a
    /// child's [`enter`](Self::enter) scope.
    #[must_use]
    pub const fn root() -> Self {
        Self(None)
    }

    /// Create a child domain with its own tick and hand-off worker thread.
    ///
    /// The worker exits once the domain is unreachable: every `Domain` handle,
    /// every cell born in it and every hand-off subscription it made has been
    /// dropped.
    #[must_use]
    pub fn child() -> Self {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let mailbox = Arc::new(Mailbox {
            inbox: Mutex::new(Inbox {
                segments: VecDeque::new(),
                applying: false,
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
        });
        DOMAINS_IN_USE.store(true, Ordering::Relaxed);
        #[cfg(not(target_arch = "wasm32"))]
        {
            let weak = Arc::downgrade(&inner);
            let spawned = std::thread::Builder::new()
                .name(format!("hyphae-domain-{id}"))
                .spawn(move || run_worker(&weak, &mailbox));
            if let Err(error) = spawned {
                log::error!("hyphae: could not start the hand-off worker of domain {id}: {error}");
            }
        }
        Self(Some(inner))
    }

    /// The domain cells born on the calling thread currently join.
    #[must_use]
    pub fn current() -> Self {
        Self(ambient_child())
    }

    pub(crate) const fn from_inner(inner: Option<Arc<DomainInner>>) -> Self {
        Self(inner)
    }

    #[must_use]
    pub const fn is_root(&self) -> bool {
        self.0.is_none()
    }

    /// Run `f` with this domain as the calling thread's ambient domain.
    ///
    /// Every cell born while `f` runs belongs to this domain, and every
    /// subscription `f` makes on a root cell is a hand-off subscription.
    /// Build a child's graph inside this scope. Cells that the child's own
    /// operators create later, while one of its waves runs, join it without a
    /// scope.
    pub fn enter<R>(&self, f: impl FnOnce() -> R) -> R {
        let _scope = enter(self.0.as_ref());
        f()
    }

    /// Run `f` as one propagation batch on this domain's tick, inside an
    /// [`enter`](Self::enter) scope. Writes `f` makes to cells of another
    /// domain are not part of the batch.
    pub fn batch<R>(&self, f: impl FnOnce() -> R) -> R {
        let _scope = enter(self.0.as_ref());
        super::batch_in(self.0.as_ref(), f)
    }

    /// Number of hand-off units queued for this domain and not yet fully
    /// applied. Always zero for root. The queue is unbounded, so a caller that
    /// writes root data faster than a child applies it can watch this to
    /// throttle itself.
    #[must_use]
    pub fn pending_hand_offs(&self) -> usize {
        self.0
            .as_ref()
            .map_or(0, |domain| domain.mailbox.pending.load(Ordering::Relaxed))
    }

    /// Block until every hand-off unit already queued for this domain has been
    /// applied and propagated. Returns at once for root.
    ///
    /// Do not call this from inside this domain's own batch or wave: the
    /// worker waits for that batch to close.
    pub fn flush(&self) {
        let Some(domain) = self.0.as_ref() else {
            return;
        };
        let mailbox = &domain.mailbox;
        let mut inbox = mailbox.inbox.lock();
        while !inbox.segments.is_empty() || inbox.applying {
            mailbox.idle.wait(&mut inbox);
        }
    }
}

impl PartialEq for Domain {
    fn eq(&self, other: &Self) -> bool {
        id_of(self.0.as_ref()) == id_of(other.0.as_ref())
    }
}

impl Eq for Domain {}

impl std::fmt::Debug for Domain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0.as_ref() {
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
