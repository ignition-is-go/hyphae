//! The settle core: how a write reaches subscribers.
//!
//! A writer pushes an event set to a lock-free input queue and tries to take
//! the one settle lock. The thread that takes it pops the event sets queued at
//! that moment, combines them into runs, and delivers each run in one
//! height-ordered pass (a settle). A thread that fails to take the lock leaves
//! its event set to the holder, and sleeps until it has settled unless it
//! used `submit` or `enqueue`.
//!
//! The specification, including the reentrancy rule and the argument for each
//! ordering guarantee, is in `docs/settle-core.md`.

use std::{
    any::Any,
    cell::{OnceCell, RefCell},
    cmp::Ordering as CmpOrdering,
    collections::{BinaryHeap, VecDeque},
    num::NonZeroU64,
    sync::{
        Arc, LazyLock, OnceLock, Weak,
        atomic::{AtomicU64, Ordering},
    },
};

use crossbeam::queue::SegQueue;
use parking_lot::Mutex;
use rustc_hash::{FxHashMap, FxHashSet};
use uuid::Uuid;

use crate::traits::DepNode;

mod wave;

pub use wave::{set_wave_min, set_workers};

/// Decides which event sets may share a settle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SettleKey {
    /// Shares a settle with any event set.
    Shared,
    /// Never shares a settle with another event set that carries the same
    /// value. Arrival order between the two is preserved.
    Exclusive(u64),
    /// Shares a settle with no other event set.
    Barrier,
}

/// Names a write and everything it causes.
///
/// A writer attaches an epoch to an event set with [`batch_with`], [`submit`],
/// or [`enqueue`]. While that set is delivered, [`current_epoch`] returns the
/// epoch in every callback, and an event set that a callback queues carries
/// it on unless the callback passes its own. A feedback chain therefore
/// keeps the epoch of the write that started it, on whichever thread each
/// link runs.
///
/// An event set with an epoch shares a settle only with event sets of the
/// same epoch. Event sets without one combine with each other as before.
///
/// The writes of an epoch can settle after the writer has returned: a
/// [`submit`] leaves them queued, and feedback takes further settles. A
/// writer that keeps state for an epoch allocates it with [`Epoch::scoped`]
/// to learn when the epoch is finished.
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
    /// event set that carries the epoch has settled. That includes a batch
    /// that is still collecting writes, and the event sets that callbacks
    /// queued while one of its sets was delivered. After that, [`current_epoch`] never returns it again unless
    /// a caller attaches it to a new write.
    ///
    /// `finished` runs once, on the thread that drops the scope or on the
    /// thread that settles the last event set. On a settling thread it is a
    /// callback like a subscriber's: it must not block, [`current_epoch`]
    /// returns `None` in it, and a write it makes is queued for a later
    /// settle.
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
            // A second panic would abort the process.
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(finished));
        } else {
            finished();
        }
    }
}

struct Scope {
    /// The scope handle, plus every event set that carries the epoch and
    /// has not settled.
    carriers: u64,
    finished: Box<dyn FnOnce() + Send>,
}

static SCOPES: LazyLock<Mutex<FxHashMap<Epoch, Scope>>> = LazyLock::new(Mutex::default);

/// Count one more event set that carries `epoch`: one being collected,
/// queued, or settling.
fn carry(epoch: Option<Epoch>) {
    if let Some(epoch) = epoch
        && epoch.0.get() & SCOPED != 0
        && let Some(scope) = SCOPES.lock().get_mut(&epoch)
    {
        scope.carriers = scope.carriers.saturating_add(1);
    }
}

/// Take `carriers` off `epoch`. Returns its callback when none is left.
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

/// A node as the frontier and the dependent edges see it.
///
/// Public only because it appears in [`DepNode::add_dependent`]. It is not
/// stable API.
#[doc(hidden)]
pub trait Node: Send + Sync {
    fn node_id(&self) -> Uuid;
    /// `1 + max(height of each dependency)`; an input is 0.
    fn node_height(&self) -> u64;
    /// Take this node's one place in the frontier. False when it already
    /// has one. The node gives the place up when it fires.
    fn claim(&self) -> bool;
    /// Deliver everything pending on this node.
    fn fire(&self);
    /// Mark this node's cached height stale.
    fn bump_height_epoch(&self);
    /// The nodes whose height depends on this one. Dead entries are pruned.
    fn node_dependents(&self) -> Vec<Weak<dyn Node>>;
}

/// One typed write, queued until a settle seeds it.
pub(crate) trait Write: Send {
    fn apply(self: Box<Self>);
    /// The node the write seeds, or `None` when it no longer exists.
    fn node(&self) -> Option<Arc<dyn Node>>;
}

struct EventSet {
    barrier: bool,
    keys: Vec<u64>,
    epoch: Option<Epoch>,
    writes: Vec<Box<dyn Write>>,
    /// The writers blocked until this set has settled: the one that
    /// submitted it, or, for a set a callback wrote during a settle, the
    /// writers that were waiting on that settle.
    waiters: Vec<Arc<Waiter>>,
    /// When the set was queued, from [`now_nanos`].
    queued_at: u64,
    /// The stint that held the settle lock when the set was queued, or 0.
    behind: u64,
}

/// Lets a writer that lost the settle lock sleep until the holder has settled
/// its event set.
struct Waiter {
    /// Event sets that must settle before the writer wakes: its own, plus
    /// every set that a callback wrote while one of those was settling.
    outstanding: AtomicU64,
    /// Order of creation, so a hand-over can pick the oldest writer.
    seq: u64,
    class: SetClass,
    /// Event sets a callback queued that this writer then also waited on.
    requeues: AtomicU64,
    state: Mutex<WaiterState>,
    settled: parking_lot::Condvar,
}

#[derive(Default)]
struct WaiterState {
    done: bool,
    /// A holder ended its stint with work still queued and asked this
    /// writer to continue the settling.
    drive: bool,
    /// When a holder asked this writer to drive, from [`now_nanos`].
    asked_at: u64,
    panic: Option<Box<dyn Any + Send>>,
}

impl Waiter {
    fn new(class: SetClass) -> Arc<Self> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        Arc::new(Self {
            outstanding: AtomicU64::new(1),
            seq: NEXT.fetch_add(1, Ordering::Relaxed),
            class,
            requeues: AtomicU64::new(0),
            state: Mutex::new(WaiterState::default()),
            settled: parking_lot::Condvar::new(),
        })
    }

    /// One of the event sets this writer waits on has settled.
    fn settled_one(&self, panic: Option<Box<dyn Any + Send>>) {
        let last = self.outstanding.fetch_sub(1, Ordering::AcqRel) == 1;
        let mut state = self.state.lock();
        if state.panic.is_none() {
            state.panic = panic;
        }
        if last {
            state.done = true;
            drop(state);
            self.settled.notify_one();
        }
    }

    /// Sleep until every set this writer waits on has settled. A holder may
    /// wake the writer early to take over the settling; it then settles one
    /// stint and goes back to sleep.
    fn wait(self: &Arc<Self>) -> Option<Box<dyn Any + Send>> {
        debug_assert!(
            ACTIVE.with_borrow(Option::is_none),
            "the settle holder must never wait on its own settle"
        );
        {
            let mut state = self.state.lock();
            if state.done {
                self.count_requeues();
                return state.panic.take();
            }
        }
        WAITING.lock().push(Arc::clone(self));
        LOCK_WAITS.fetch_add(1, Ordering::Relaxed);
        let mut drive_panic = None;
        let mut state = self.state.lock();
        loop {
            if state.drive {
                state.drive = false;
                let asked_at = state.asked_at;
                drop(state);
                HANDOVER_WAKE_NANOS
                    .fetch_add(now_nanos().saturating_sub(asked_at), Ordering::Relaxed);
                // Always settle once after being asked, even if this
                // writer's own set finished meanwhile: the holder that asked
                // has already left.
                let outcome =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drain(None)));
                if let Err(payload) = outcome
                    && drive_panic.is_none()
                {
                    drive_panic = Some(payload);
                }
                state = self.state.lock();
                continue;
            }
            if state.done {
                break;
            }
            self.settled.wait(&mut state);
        }
        let panic = state.panic.take().or(drive_panic);
        drop(state);
        WAITING.lock().retain(|waiter| !Arc::ptr_eq(waiter, self));
        self.count_requeues();
        panic
    }

    fn count_requeues(&self) {
        let counters = self.class.counters();
        counters.waits.fetch_add(1, Ordering::Relaxed);
        counters
            .requeue_rounds
            .fetch_add(self.requeues.load(Ordering::Relaxed), Ordering::Relaxed);
    }

    /// Ask this writer to continue the settling. False when it has already
    /// been asked or has finished waiting.
    fn ask_to_drive(&self) -> bool {
        let mut state = self.state.lock();
        if state.done || state.drive {
            return false;
        }
        state.drive = true;
        state.asked_at = now_nanos();
        drop(state);
        self.settled.notify_one();
        true
    }
}

impl EventSet {
    fn new() -> Self {
        Self {
            barrier: false,
            keys: Vec::new(),
            epoch: None,
            writes: Vec::new(),
            waiters: Vec::new(),
            queued_at: 0,
            behind: 0,
        }
    }

    fn class(&self) -> SetClass {
        if self.writes.len() <= PROBE_WRITES {
            SetClass::Probe
        } else {
            SetClass::Large
        }
    }

    fn add_key(&mut self, key: SettleKey) {
        match key {
            SettleKey::Shared => {}
            SettleKey::Exclusive(key) => {
                if !self.keys.contains(&key) {
                    self.keys.push(key);
                }
            }
            SettleKey::Barrier => self.barrier = true,
        }
    }
}

static INPUT: SegQueue<EventSet> = SegQueue::new();
/// Event sets a holder popped between waves and could not settle ahead.
/// They arrived before everything still in [`INPUT`], so the next stint
/// takes them first. Only the holder touches them.
static CARRY: Mutex<VecDeque<EventSet>> = Mutex::new(VecDeque::new());
/// The writers now asleep in [`Waiter::wait`].
static WAITING: Mutex<Vec<Arc<Waiter>>> = Mutex::new(Vec::new());
static SETTLE: Mutex<()> = Mutex::new(());

/// Which fire produced a value, for a node that must deliver the values of
/// one settle in the order a single thread would have produced them.
///
/// Stamps order the fires of one settle: lower height first, then earlier
/// arrival in the frontier. The order does not depend on which thread fired a
/// node or when.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Stamp {
    height: u64,
    seq: u64,
}

impl Stamp {
    /// A value seeded from an event set, or written outside a settle.
    pub(crate) const SEED: Self = Self { height: 0, seq: 0 };
    /// Above every node. A write made under it never joins the run, whose
    /// frontier is already empty.
    const AFTER_RUN: Self = Self {
        height: u64::MAX,
        seq: 0,
    };
}

/// The stamp of the node now firing on this thread, or [`Stamp::SEED`].
pub(crate) fn stamp() -> Stamp {
    ACTIVE.with_borrow(|active| {
        active
            .as_ref()
            .and_then(|active| active.firing)
            .unwrap_or(Stamp::SEED)
    })
}

struct Entry {
    height: u64,
    /// Arrival order in the frontier. Starts at 1, so no entry's stamp
    /// equals [`Stamp::SEED`].
    seq: u64,
    node: Weak<dyn Node>,
    /// The event sets of the run whose values this node carries; filled in
    /// when the entry is popped. See [`Tags`].
    tags: Tags,
}

impl PartialEq for Entry {
    fn eq(&self, other: &Self) -> bool {
        self.height == other.height && self.seq == other.seq
    }
}
impl Eq for Entry {}
impl PartialOrd for Entry {
    fn partial_cmp(&self, other: &Self) -> Option<CmpOrdering> {
        Some(self.cmp(other))
    }
}
impl Ord for Entry {
    // Reversed: `BinaryHeap` is a max-heap and the frontier pops the lowest
    // height first, then the earliest arrival.
    fn cmp(&self, other: &Self) -> CmpOrdering {
        (other.height, other.seq).cmp(&(self.height, self.seq))
    }
}

/// A bit per waited-on event set of a run: bit `i` for the `i`th such set,
/// and bit 63 for the 63rd and every later one. A frontier entry carries the
/// bits of every set whose values reached it, and the writers of a set wake
/// once no entry carries its bit.
type Tags = u64;

const TAG_BITS: usize = 64;

fn tag_bit(index: usize) -> Tags {
    u32::try_from(index.min(TAG_BITS - 1))
        .ok()
        .and_then(|bit| Tags::checked_shl(1, bit))
        .unwrap_or(0)
}

/// The bits set in `tags`, lowest first.
fn bits(tags: Tags) -> impl Iterator<Item = usize> {
    (0..TAG_BITS).filter(move |&bit| {
        u32::try_from(bit)
            .ok()
            .and_then(|bit| tags.checked_shr(bit))
            .is_some_and(|shifted| shifted & 1 == 1)
    })
}

/// The address of a node, which identifies it in the frontier and in cones.
fn addr<T: ?Sized>(node: *const T) -> usize {
    node.cast::<()>().addr()
}

/// The nodes waiting to fire in the settle in progress. Only the holder
/// touches it.
struct Frontier {
    heap: BinaryHeap<Entry>,
    seq: u64,
    /// The tags of each node in the heap, by address.
    tags: FxHashMap<usize, Tags>,
    /// How many nodes in the heap carry each bit.
    counts: [u32; TAG_BITS],
    /// Bits whose values reached a node this frontier does not hold. Their
    /// writers wait for the end of the run.
    pinned: Tags,
    /// The nodes popped, when the frontier records them.
    fired: Option<FxHashSet<usize>>,
}

impl Default for Frontier {
    fn default() -> Self {
        Self {
            heap: BinaryHeap::new(),
            seq: 0,
            tags: FxHashMap::default(),
            counts: [0; TAG_BITS],
            pinned: 0,
            fired: None,
        }
    }
}

impl Frontier {
    fn push(&mut self, height: u64, node: Weak<dyn Node>, tags: Tags) {
        self.seq = self.seq.wrapping_add(1);
        self.add_tags(addr(node.as_ptr()), tags);
        self.heap.push(Entry {
            height,
            seq: self.seq,
            node,
            tags: 0,
        });
    }

    fn add_tags(&mut self, node: usize, tags: Tags) {
        let held = self.tags.entry(node).or_default();
        let new = tags & !*held;
        *held |= tags;
        for bit in bits(new) {
            if let Some(count) = self.counts.get_mut(bit) {
                *count = count.saturating_add(1);
            }
        }
    }

    /// Move every entry of the lowest height into `wave`, earliest arrival
    /// first. False when the frontier is empty.
    ///
    /// A fire only schedules nodes above its own height, so the wave is
    /// complete when it is popped.
    fn pop_wave(&mut self, wave: &mut Vec<Entry>) -> bool {
        wave.clear();
        let Some(first) = self.heap.pop() else {
            return false;
        };
        let height = first.height;
        wave.push(first);
        while self.heap.peek().is_some_and(|next| next.height == height) {
            wave.extend(self.heap.pop());
        }
        for entry in wave.iter_mut() {
            let node = addr(entry.node.as_ptr());
            if let Some(fired) = self.fired.as_mut() {
                fired.insert(node);
            }
            let tags = self.tags.remove(&node).unwrap_or(0);
            entry.tags = tags;
            for bit in bits(tags) {
                if let Some(count) = self.counts.get_mut(bit) {
                    *count = count.saturating_sub(1);
                }
            }
        }
        true
    }

    /// The bits of `live` that no node in the frontier carries.
    fn drained(&self, live: Tags) -> Tags {
        bits(live & !self.pinned)
            .filter(|&bit| self.counts.get(bit) == Some(&0))
            .fold(0, |drained, bit| drained | tag_bit(bit))
    }

    /// Take in what a stretch of fires produced. Callers pass outboxes in
    /// the order the fires would have run on one thread, which keeps `seq`
    /// and the input queue independent of thread timing.
    ///
    /// A node enters at its first write in that order. The thread that
    /// wrote to it first in time may be a later one, so the claim is made
    /// here and not by the writer. A node that is already in the frontier
    /// gains the request's tags. A node that another frontier holds, which a
    /// run that overtook another reaches outside its predicted cone, pins
    /// the tags: the value waits for that other run.
    fn merge(&mut self, out: Outbox) {
        for request in out.scheduled {
            let Some(node) = request.node.upgrade() else {
                continue;
            };
            if request.raised || node.claim() {
                self.push(request.height, request.node, request.tags);
            } else if self.tags.contains_key(&addr(request.node.as_ptr())) {
                self.add_tags(addr(request.node.as_ptr()), request.tags);
            } else {
                self.pinned |= request.tags;
                self.pinned |= ESCAPED;
            }
        }
        for mut set in out.deferred {
            set.behind = HOLDING.load(Ordering::Relaxed);
            count_push(&set);
            INPUT.push(set);
        }
    }

    const fn escaped(&self) -> bool {
        self.pinned & ESCAPED != 0
    }
}

/// Set in [`Frontier::pinned`] when a request found its node held by
/// another frontier. Shares bit 63, whose writers wake at the end of a run.
const ESCAPED: Tags = 1 << (TAG_BITS - 1);

/// What the fires on one thread produced since the outbox was last taken.
#[derive(Default)]
struct Outbox {
    /// One request per write that joined the settle, in write order. A
    /// node can appear many times.
    scheduled: Vec<Request>,
    /// Event sets that callbacks queued.
    deferred: Vec<EventSet>,
}

/// A request to put `node` in the frontier at `height`.
struct Request {
    height: u64,
    node: Weak<dyn Node>,
    /// The node was popped at a height it has since outgrown. It still
    /// holds its place in the frontier, so it goes back without a claim.
    raised: bool,
    /// The tags of the fire, or the seeding set, that made the request.
    tags: Tags,
}

/// The writers of a run by tag bit. A writer appears once per event set it
/// waits on.
type Groups = Arc<[Vec<Arc<Waiter>>]>;

/// This thread's part in the settle in progress: the holder's, or that of a
/// pool thread firing part of a wave.
struct Active {
    /// The node now firing; `None` while a run is being seeded.
    firing: Option<Stamp>,
    /// The tags of the node now firing, or of the set being seeded.
    tags: Tags,
    run_id: u64,
    /// The epoch of the run now settling.
    epoch: Option<Epoch>,
    /// The writers waiting on the run now settling.
    groups: Groups,
    out: Outbox,
}

impl Active {
    fn new(run_id: u64, epoch: Option<Epoch>, groups: Groups) -> Self {
        Self {
            firing: None,
            tags: 0,
            run_id,
            epoch,
            groups,
            out: Outbox::default(),
        }
    }

    /// The distinct writers of the sets in `self.tags`.
    fn waiters(&self) -> Vec<Arc<Waiter>> {
        let mut distinct: Vec<Arc<Waiter>> = Vec::new();
        for bit in bits(self.tags) {
            for waiter in self.groups.get(bit).into_iter().flatten() {
                if !distinct.iter().any(|seen| Arc::ptr_eq(seen, waiter)) {
                    distinct.push(Arc::clone(waiter));
                }
            }
        }
        distinct
    }
}

fn take_outbox() -> Outbox {
    ACTIVE.with_borrow_mut(|active| {
        active
            .as_mut()
            .map(|active| std::mem::take(&mut active.out))
            .unwrap_or_default()
    })
}

struct Building {
    depth: u32,
    set: EventSet,
}

/// What a writer does after it has queued an event set.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Mode {
    /// Settle it, or sleep until the holder has.
    Wait,
    /// Settle it if the lock is free; otherwise leave it to the holder.
    Submit,
    /// Leave it queued. Some later settle delivers it.
    Enqueue,
}

thread_local! {
    /// How a write on this thread ends, set by [`submit`] and [`enqueue`].
    static MODE: std::cell::Cell<Mode> = const { std::cell::Cell::new(Mode::Wait) };
    static ACTIVE: RefCell<Option<Active>> = const { RefCell::new(None) };
    static BUILDING: RefCell<Option<Building>> = const { RefCell::new(None) };
}

static RUN_ID: AtomicU64 = AtomicU64::new(0);

/// Process-wide totals since start. See [`stats`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SettleStats {
    /// Event sets submitted.
    pub event_sets: u64,
    /// Settles run.
    pub runs: u64,
    /// Fanouts delivered.
    pub fires: u64,
    /// Fanouts by a node that had already delivered one in the same settle.
    pub duplicate_fires: u64,
    /// Submissions that found the settle lock held.
    pub lock_misses: u64,
    /// Writers that slept until a holder had settled what they waited on.
    pub lock_waits: u64,
    /// Writes made during a settle that became a later event set.
    pub deferred_writes: u64,
    /// Nanoseconds spent holding the settle lock.
    pub settle_nanos: u64,
    /// The longest single hold of the settle lock, in nanoseconds.
    pub max_hold_nanos: u64,
    /// Times a holder left work queued and woke a sleeping writer to continue.
    pub handovers: u64,
    /// Empty event sets that [`flush`] queued to wait behind earlier ones.
    pub flush_sets: u64,
    /// Waves fired. A wave is the nodes of one height in one settle.
    pub waves: u64,
    /// Waves fired on pool threads.
    pub parallel_waves: u64,
    /// The most nodes in one wave.
    pub widest_wave: u64,
    /// Times an event set started a new settle only because its epoch
    /// differed from the one before it.
    pub epoch_splits: u64,
    /// Event sets that settled in a run of their own ahead of earlier
    /// queued sets, because no node they reach is reached by those sets.
    pub overtakes: u64,
    /// Times a run that overtook reached a node outside its predicted cone
    /// that an earlier run holds or fires.
    pub cone_escapes: u64,
    /// Writers woken before the end of their run, when the nodes their
    /// event set reaches had fired.
    pub early_wakes: u64,
    /// Nanoseconds from a holder asking a sleeping writer to take over to
    /// that writer starting to settle, summed.
    pub handover_wake_nanos: u64,
    /// Event sets of at most [`PROBE_WRITES`] writes.
    pub probe: SetLatency,
    /// Event sets of more writes.
    pub large: SetLatency,
}

/// The largest event set, in writes, that [`SettleStats::probe`] counts.
pub const PROBE_WRITES: usize = 8;

/// Where the time of one class of event set goes, summed over its sets.
/// Every field is a total; divide by `sets` or `waits` for a mean.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SetLatency {
    /// Event sets with writes that a run settled.
    pub sets: u64,
    /// Nanoseconds from queueing until the stint that held the settle lock
    /// then released it.
    pub wait_behind_current_nanos: u64,
    /// Nanoseconds from that release, or from queueing when the lock was
    /// free, until the set's own run started.
    pub wait_behind_queue_nanos: u64,
    /// Queued event sets ahead of each set when it was pushed.
    pub queue_len_at_push: u64,
    /// Nanoseconds from the start of each set's run until its writers woke
    /// or the run ended.
    pub own_run_nanos: u64,
    /// Fanouts in each set's run.
    pub own_run_fires: u64,
    /// Sets whose forward cone was computed.
    pub cones: u64,
    /// Nodes in those cones.
    pub cone_nodes: u64,
    /// Writers that waited on a set of this class.
    pub waits: u64,
    /// Event sets that feedback queued and those writers then waited on.
    pub requeue_rounds: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SetClass {
    Probe,
    Large,
}

impl SetClass {
    fn counters(self) -> &'static LatencyCounters {
        static PROBE: LatencyCounters = LatencyCounters::new();
        static LARGE: LatencyCounters = LatencyCounters::new();
        match self {
            Self::Probe => &PROBE,
            Self::Large => &LARGE,
        }
    }
}

struct LatencyCounters {
    sets: AtomicU64,
    wait_behind_current_nanos: AtomicU64,
    wait_behind_queue_nanos: AtomicU64,
    queue_len_at_push: AtomicU64,
    own_run_nanos: AtomicU64,
    own_run_fires: AtomicU64,
    cones: AtomicU64,
    cone_nodes: AtomicU64,
    waits: AtomicU64,
    requeue_rounds: AtomicU64,
}

impl LatencyCounters {
    const fn new() -> Self {
        Self {
            sets: AtomicU64::new(0),
            wait_behind_current_nanos: AtomicU64::new(0),
            wait_behind_queue_nanos: AtomicU64::new(0),
            queue_len_at_push: AtomicU64::new(0),
            own_run_nanos: AtomicU64::new(0),
            own_run_fires: AtomicU64::new(0),
            cones: AtomicU64::new(0),
            cone_nodes: AtomicU64::new(0),
            waits: AtomicU64::new(0),
            requeue_rounds: AtomicU64::new(0),
        }
    }

    fn read(&self) -> SetLatency {
        SetLatency {
            sets: self.sets.load(Ordering::Relaxed),
            wait_behind_current_nanos: self.wait_behind_current_nanos.load(Ordering::Relaxed),
            wait_behind_queue_nanos: self.wait_behind_queue_nanos.load(Ordering::Relaxed),
            queue_len_at_push: self.queue_len_at_push.load(Ordering::Relaxed),
            own_run_nanos: self.own_run_nanos.load(Ordering::Relaxed),
            own_run_fires: self.own_run_fires.load(Ordering::Relaxed),
            cones: self.cones.load(Ordering::Relaxed),
            cone_nodes: self.cone_nodes.load(Ordering::Relaxed),
            waits: self.waits.load(Ordering::Relaxed),
            requeue_rounds: self.requeue_rounds.load(Ordering::Relaxed),
        }
    }
}

static OVERTAKES: AtomicU64 = AtomicU64::new(0);
static CONE_ESCAPES: AtomicU64 = AtomicU64::new(0);
static EARLY_WAKES: AtomicU64 = AtomicU64::new(0);
static HANDOVER_WAKE_NANOS: AtomicU64 = AtomicU64::new(0);

/// Nanoseconds since the first call.
fn now_nanos() -> u64 {
    static START: LazyLock<crate::platform::Instant> = LazyLock::new(crate::platform::Instant::now);
    u64::try_from(START.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

/// The stint now holding the settle lock, or 0.
static HOLDING: AtomicU64 = AtomicU64::new(0);
static STINTS: AtomicU64 = AtomicU64::new(0);
const RELEASE_RING: usize = 1024;
/// When recent stints released the lock, by stint number modulo the ring.
static RELEASES: [(AtomicU64, AtomicU64); RELEASE_RING] =
    [const { (AtomicU64::new(0), AtomicU64::new(0)) }; RELEASE_RING];

fn release_slot(stint: u64) -> Option<&'static (AtomicU64, AtomicU64)> {
    RELEASES.get(usize::try_from(stint).ok()?.checked_rem(RELEASE_RING)?)
}

fn record_release(stint: u64) {
    if let Some(slot) = release_slot(stint) {
        slot.1.store(now_nanos(), Ordering::Relaxed);
        slot.0.store(stint, Ordering::Release);
    }
}

/// When stint `stint` released the lock, if the ring still holds it.
fn released_at(stint: u64) -> Option<u64> {
    let slot = release_slot(stint)?;
    (stint != 0 && slot.0.load(Ordering::Acquire) == stint).then(|| slot.1.load(Ordering::Relaxed))
}

/// Count a set about to enter the input queue.
fn count_push(set: &EventSet) {
    if set.writes.is_empty() {
        return;
    }
    let len = u64::try_from(INPUT.len()).unwrap_or(u64::MAX);
    set.class()
        .counters()
        .queue_len_at_push
        .fetch_add(len, Ordering::Relaxed);
}

/// Count the wait of each set of a run that starts at `start`.
fn count_run_start(run: &[EventSet], start: u64) {
    for set in run.iter().filter(|set| !set.writes.is_empty()) {
        let counters = set.class().counters();
        counters.sets.fetch_add(1, Ordering::Relaxed);
        let (current, queue) = released_at(set.behind).map_or_else(
            || (0, start.saturating_sub(set.queued_at)),
            |released| {
                (
                    released.saturating_sub(set.queued_at),
                    start.saturating_sub(released),
                )
            },
        );
        counters
            .wait_behind_current_nanos
            .fetch_add(current, Ordering::Relaxed);
        counters
            .wait_behind_queue_nanos
            .fetch_add(queue, Ordering::Relaxed);
    }
}

static EVENT_SETS: AtomicU64 = AtomicU64::new(0);
static RUNS: AtomicU64 = AtomicU64::new(0);
static FIRES: AtomicU64 = AtomicU64::new(0);
static DUPLICATE_FIRES: AtomicU64 = AtomicU64::new(0);
static LOCK_MISSES: AtomicU64 = AtomicU64::new(0);
static LOCK_WAITS: AtomicU64 = AtomicU64::new(0);
static DEFERRED_WRITES: AtomicU64 = AtomicU64::new(0);
static SETTLE_NANOS: AtomicU64 = AtomicU64::new(0);
static MAX_HOLD_NANOS: AtomicU64 = AtomicU64::new(0);
static HANDOVERS: AtomicU64 = AtomicU64::new(0);
static FLUSH_SETS: AtomicU64 = AtomicU64::new(0);
static WAVES: AtomicU64 = AtomicU64::new(0);
static PARALLEL_WAVES: AtomicU64 = AtomicU64::new(0);
static WIDEST_WAVE: AtomicU64 = AtomicU64::new(0);
static EPOCH_SPLITS: AtomicU64 = AtomicU64::new(0);

/// Read the process-wide settle counters.
#[must_use]
pub fn stats() -> SettleStats {
    SettleStats {
        event_sets: EVENT_SETS.load(Ordering::Relaxed),
        runs: RUNS.load(Ordering::Relaxed),
        fires: FIRES.load(Ordering::Relaxed),
        duplicate_fires: DUPLICATE_FIRES.load(Ordering::Relaxed),
        lock_misses: LOCK_MISSES.load(Ordering::Relaxed),
        lock_waits: LOCK_WAITS.load(Ordering::Relaxed),
        deferred_writes: DEFERRED_WRITES.load(Ordering::Relaxed),
        settle_nanos: SETTLE_NANOS.load(Ordering::Relaxed),
        max_hold_nanos: MAX_HOLD_NANOS.load(Ordering::Relaxed),
        handovers: HANDOVERS.load(Ordering::Relaxed),
        flush_sets: FLUSH_SETS.load(Ordering::Relaxed),
        waves: WAVES.load(Ordering::Relaxed),
        parallel_waves: PARALLEL_WAVES.load(Ordering::Relaxed),
        widest_wave: WIDEST_WAVE.load(Ordering::Relaxed),
        epoch_splits: EPOCH_SPLITS.load(Ordering::Relaxed),
        overtakes: OVERTAKES.load(Ordering::Relaxed),
        cone_escapes: CONE_ESCAPES.load(Ordering::Relaxed),
        early_wakes: EARLY_WAKES.load(Ordering::Relaxed),
        handover_wake_nanos: HANDOVER_WAKE_NANOS.load(Ordering::Relaxed),
        probe: SetClass::Probe.counters().read(),
        large: SetClass::Large.counters().read(),
    }
}

/// Count one fanout by a node whose last fanout was in run `fired_run`.
pub(crate) fn record_fire(fired_run: &AtomicU64) {
    let run = current_run();
    FIRES.fetch_add(1, Ordering::Relaxed);
    if fired_run.swap(run, Ordering::Relaxed) == run {
        DUPLICATE_FIRES.fetch_add(1, Ordering::Relaxed);
    }
}

fn current_run() -> u64 {
    ACTIVE.with_borrow(|active| active.as_ref().map_or(0, |active| active.run_id))
}

/// The epoch of the event set now being delivered on this thread, or `None`
/// outside a settle and for an event set without one. See [`Epoch`].
#[must_use]
pub fn current_epoch() -> Option<Epoch> {
    ACTIVE.with_borrow(|active| active.as_ref().and_then(|active| active.epoch))
}

/// True on a thread that is delivering an event set: inside a subscriber
/// callback, on the holder or on a settle pool thread. A write made there is
/// placed by the reentrancy rule and never waits.
#[must_use]
pub fn is_settling() -> bool {
    ACTIVE.with_borrow(Option::is_some)
}

/// Where a write made right now belongs.
pub(crate) enum Position {
    /// It joins the settle in progress on this thread.
    Joins,
    /// It must go through the input queue. The write keeps the guard, if
    /// any, until a settle seeds it.
    Queue(Option<Backlogged>),
}

/// The writes to one node that a callback queued and no settle has seeded
/// yet.
///
/// While there is one, a later write to the node from a callback is queued
/// behind it, even when the node's height would let it join the settle. A
/// node that gains a dependency rises, so a write that had to wait can be
/// followed by one that could join, and the second would be delivered first.
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

/// One write in a [`Backlog`]. It leaves when it is dropped: after a settle
/// has seeded the write, or with a write that is never seeded.
pub(crate) struct Backlogged(Arc<AtomicU64>);

impl Drop for Backlogged {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Decide whether a write to a node of height `target` joins the settle in
/// progress on this thread.
///
/// It joins when a run is being seeded. From a callback it joins when the
/// target is strictly higher than the node now firing, so the target has not
/// fired yet in this settle, and no earlier write to the target is still
/// queued in `backlog`. A write inside a [`batch`] never joins: the batch
/// keeps its writes together.
pub(crate) fn position(target: impl FnOnce() -> u64, backlog: &Backlog) -> Position {
    let Some(firing) = ACTIVE.with_borrow(|active| active.as_ref().map(|active| active.firing))
    else {
        return Position::Queue(None);
    };
    if BUILDING.with_borrow(Option::is_some) {
        return Position::Queue(Some(backlog.add()));
    }
    match firing {
        None => Position::Joins,
        Some(firing) if backlog.is_empty() && target() > firing.height => Position::Joins,
        Some(_) => Position::Queue(Some(backlog.add())),
    }
}

/// Ask for `node` to fire in the settle in progress on this thread. Call it
/// for every write that joins the settle; the node still fires once.
pub(crate) fn schedule(node: Weak<dyn Node>, height: u64) {
    let tags = ACTIVE.with_borrow(|active| active.as_ref().map_or(0, |active| active.tags));
    request(Request {
        height,
        node,
        raised: false,
        tags,
    });
}

fn request(request: Request) {
    ACTIVE.with_borrow_mut(|active| {
        if let Some(active) = active.as_mut() {
            let scheduled = &mut active.out.scheduled;
            // One fanout often writes many values to one target.
            let repeats = scheduled.last().is_some_and(|last| {
                !last.raised && last.tags == request.tags && last.node.ptr_eq(&request.node)
            });
            if request.raised || !repeats {
                scheduled.push(request);
            }
        }
    });
}

/// Submit one write as its own event set, or add it to the batch this thread
/// is building.
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
    let mut set = EventSet::new();
    set.writes.push(write);
    set.add_key(key);
    push(set);
}

/// Queue `set`, then settle it or wait for it as the thread's mode asks.
fn push(set: EventSet) {
    queue(set).finish();
}

/// An event set in the input queue whose writer has not yet settled it or
/// waited on it. Dropping it leaves the set to some later settle.
#[must_use = "call `finish`, or the writer returns before its write has settled"]
pub(crate) struct Queued {
    /// False when the thread never settles this set itself: it is firing a
    /// node, or it used [`enqueue`].
    settles: bool,
    waiter: Option<Arc<Waiter>>,
}

impl Queued {
    const LEFT_QUEUED: Self = Self {
        settles: false,
        waiter: None,
    };

    /// Settle the set if the lock is free, and sleep until it has settled
    /// unless the thread used [`submit`] when it queued the set.
    pub(crate) fn finish(self) {
        if !self.settles {
            return;
        }
        if let Some(guard) = SETTLE.try_lock() {
            drain(Some(guard));
        } else {
            LOCK_MISSES.fetch_add(1, Ordering::Relaxed);
            // The holder may have released since the failed try, in which
            // case nobody is coming for this set.
            drain(None);
        }
        // Whoever holds the lock re-checks the queue after releasing it, so
        // some holder settles what this writer waits on and wakes it.
        if let Some(waiter) = self.waiter
            && let Some(payload) = waiter.wait()
        {
            std::panic::resume_unwind(payload);
        }
    }
}

fn queue(mut set: EventSet) -> Queued {
    EVENT_SETS.fetch_add(1, Ordering::Relaxed);
    set.queued_at = now_nanos();
    set.behind = HOLDING.load(Ordering::Relaxed);
    if ACTIVE.with_borrow(Option::is_some) {
        // Reentrant: this thread is firing a node, so it never waits. The
        // set reaches the input queue when the holder merges this thread's
        // outbox, and a later stint settles it. A writer waiting on the
        // settle in progress waits for this set too, so it wakes to the
        // settled result of its write and not to a half-finished chain.
        DEFERRED_WRITES.fetch_add(1, Ordering::Relaxed);
        ACTIVE.with_borrow_mut(|active| {
            if let Some(active) = active.as_mut() {
                // An empty set writes nothing, so it has no cause to name.
                if set.epoch.is_none() && !set.writes.is_empty() {
                    set.epoch = active.epoch;
                    carry(set.epoch);
                }
                set.waiters = active.waiters();
                for waiter in &set.waiters {
                    waiter.outstanding.fetch_add(1, Ordering::AcqRel);
                    waiter.requeues.fetch_add(1, Ordering::Relaxed);
                }
                active.out.deferred.push(set);
            }
        });
        return Queued::LEFT_QUEUED;
    }
    let mode = MODE.get();
    // A stint is bounded, so even a writer that takes the lock can leave with
    // the sets its callbacks queued still unsettled. It waits for those too.
    let waiter = (mode == Mode::Wait).then(|| Waiter::new(set.class()));
    set.waiters.extend(waiter.iter().cloned());
    count_push(&set);
    INPUT.push(set);
    Queued {
        settles: mode != Mode::Enqueue,
        waiter,
    }
}

/// Settle until the input queue is empty, starting with `guard` when the
/// caller already holds the lock. Returns when another thread holds the lock:
/// that thread re-checks the queue after it releases, so nothing queued is
/// stranded.
fn drain(mut guard: Option<parking_lot::MutexGuard<'static, ()>>) {
    let mut first_panic: Option<Box<dyn Any + Send>> = None;
    loop {
        let Some(held) = guard.take().or_else(|| SETTLE.try_lock()) else {
            break;
        };
        let started = crate::platform::Instant::now();
        let stint = STINTS
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1)
            .max(1);
        HOLDING.store(stint, Ordering::Relaxed);
        ACTIVE.set(Some(Active::new(0, None, Arc::default())));
        let panic = settle_queued();
        ACTIVE.set(None);
        let nanos = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
        SETTLE_NANOS.fetch_add(nanos, Ordering::Relaxed);
        MAX_HOLD_NANOS.fetch_max(nanos, Ordering::Relaxed);
        HOLDING.store(0, Ordering::Relaxed);
        record_release(stint);
        drop(held);
        if first_panic.is_none() {
            first_panic = panic;
        }
        // A stint is bounded, and a writer may have pushed after the last pop
        // and missed the lock before the release above. Either way nobody
        // else is coming for what is queued, unless a sleeping writer takes
        // over. Handing over keeps one thread from settling for everyone.
        if INPUT.is_empty() && CARRY.lock().is_empty() {
            break;
        }
        if hand_over() {
            HANDOVERS.fetch_add(1, Ordering::Relaxed);
            break;
        }
        std::thread::yield_now();
    }
    if let Some(payload) = first_panic {
        std::panic::resume_unwind(payload);
    }
}

/// Ask the writer that has waited longest to continue the settling. Its set
/// is the most likely to be at the head of the queue.
fn hand_over() -> bool {
    let mut waiting = WAITING.lock().clone();
    waiting.sort_unstable_by_key(|waiter| waiter.seq);
    waiting.iter().any(|waiter| waiter.ask_to_drive())
}

/// Node addresses in a forward cone.
type Cone = FxHashSet<usize>;

/// A cone larger than this counts as unknown.
const MAX_CONE: usize = 1 << 16;

/// Bumped whenever an edge is added, which can grow any cone.
static CONE_GEN: AtomicU64 = AtomicU64::new(0);

#[derive(Default)]
struct ConeCache {
    generation: u64,
    cones: FxHashMap<usize, Option<Arc<Cone>>>,
}

static CONES: LazyLock<Mutex<ConeCache>> = LazyLock::new(Mutex::default);

/// Drop every cached cone. Called when a node gains a dependent.
pub(crate) fn edges_changed() {
    CONE_GEN.fetch_add(1, Ordering::AcqRel);
}

/// `node` and every node that depends on it, transitively, or `None` when
/// that is too many.
///
/// The cone follows dependency edges as they are when it is walked. A
/// subscriber callback that writes to a node it does not own, and an edge
/// added later, reach outside it; see [`Frontier::merge`]. A cone walked
/// while an edge was added is used but not cached.
fn cone_of(node: &Arc<dyn Node>) -> Option<Arc<Cone>> {
    let key = addr(Arc::as_ptr(node));
    let generation = CONE_GEN.load(Ordering::Acquire);
    {
        let cache = CONES.lock();
        if cache.generation == generation
            && let Some(cone) = cache.cones.get(&key)
        {
            return cone.clone();
        }
    }
    let mut cone = Cone::default();
    cone.insert(key);
    let mut stack = vec![Arc::clone(node)];
    let mut known = true;
    while let Some(node) = stack.pop() {
        for dependent in node.node_dependents() {
            if cone.insert(addr(dependent.as_ptr())) {
                if cone.len() > MAX_CONE {
                    known = false;
                    stack.clear();
                    break;
                }
                stack.extend(dependent.upgrade());
            }
        }
    }
    let cone = known.then(|| Arc::new(cone));
    let mut cache = CONES.lock();
    if CONE_GEN.load(Ordering::Acquire) != generation {
        return cone;
    }
    if cache.generation != generation {
        cache.cones.clear();
        cache.generation = generation;
    }
    cache.cones.insert(key, cone.clone());
    cone
}

/// The union of the cones of `nodes`, or `None` when one is unknown.
fn union_cone(nodes: impl IntoIterator<Item = Arc<dyn Node>>) -> Option<Arc<Cone>> {
    let mut union: Option<Arc<Cone>> = None;
    for node in nodes {
        let cone = cone_of(&node)?;
        union = Some(match union {
            None => cone,
            Some(mut union) => {
                Arc::make_mut(&mut union).extend(cone.iter().copied());
                union
            }
        });
    }
    Some(union.unwrap_or_default())
}

/// How many of `sets`, from the front, form one run. A run closes before a
/// set that carries one of its exclusive keys, before or after a barrier,
/// and before a set of another epoch.
fn run_len<'a>(sets: impl IntoIterator<Item = &'a EventSet>) -> usize {
    let mut len = 0usize;
    let mut keys: FxHashSet<u64> = FxHashSet::default();
    let mut barrier = false;
    let mut epoch: Option<Option<Epoch>> = None;
    for set in sets {
        let conflicts =
            len > 0 && (barrier || set.barrier || set.keys.iter().any(|key| keys.contains(key)));
        // A run has one epoch, so that `current_epoch` names one write. An
        // empty set, which `flush` queues, writes nothing and fits any run.
        let writes = !set.writes.is_empty();
        let other_epoch = writes && epoch.is_some_and(|epoch| epoch != set.epoch);
        if other_epoch && !conflicts {
            EPOCH_SPLITS.fetch_add(1, Ordering::Relaxed);
        }
        if conflicts || other_epoch {
            break;
        }
        if writes {
            epoch = Some(set.epoch);
        }
        barrier |= set.barrier;
        keys.extend(set.keys.iter().copied());
        len = len.saturating_add(1);
    }
    len
}

/// A popped event set that has not settled.
struct Held {
    set: EventSet,
    /// Popped when the stint began. Only these settle in order in the
    /// stint; a set popped between waves settles in it only by overtaking.
    initial: bool,
    /// The set's forward cone once computed; `None` inside when unknown.
    cone: OnceCell<Option<Arc<Cone>>>,
}

impl Held {
    const fn new(set: EventSet, initial: bool) -> Self {
        Self {
            set,
            initial,
            cone: OnceCell::new(),
        }
    }

    fn cone(&self) -> Option<&Cone> {
        let set = &self.set;
        self.cone
            .get_or_init(|| {
                let cone = union_cone(set.writes.iter().filter_map(|write| write.node()));
                if let Some(cone) = &cone {
                    let counters = set.class().counters();
                    counters.cones.fetch_add(1, Ordering::Relaxed);
                    counters.cone_nodes.fetch_add(
                        u64::try_from(cone.len()).unwrap_or(u64::MAX),
                        Ordering::Relaxed,
                    );
                }
                cone
            })
            .as_deref()
    }
}

/// The run now firing, as a set that might overtake it sees it.
struct InFlight {
    seeds: Vec<Arc<dyn Node>>,
    keys: FxHashSet<u64>,
    barrier: bool,
    cone: OnceCell<Option<Arc<Cone>>>,
}

impl InFlight {
    fn new(run: &[EventSet]) -> Self {
        Self {
            seeds: run
                .iter()
                .flat_map(|set| set.writes.iter().filter_map(|write| write.node()))
                .collect(),
            keys: run
                .iter()
                .flat_map(|set| set.keys.iter().copied())
                .collect(),
            barrier: run.iter().any(|set| set.barrier),
            cone: OnceCell::new(),
        }
    }

    fn cone(&mut self) -> Option<Arc<Cone>> {
        let seeds = &mut self.seeds;
        self.cone
            .get_or_init(|| union_cone(std::mem::take(seeds)))
            .clone()
    }
}

/// How far a scan for sets that may overtake the run in flight has got.
#[derive(Default)]
struct Scan {
    /// Held sets already looked at. None of them may overtake.
    scanned: usize,
    /// Their cones and keys, which a later set must not touch.
    cone: Cone,
    keys: FxHashSet<u64>,
    /// One of them is a barrier or has an unknown cone, so nothing after it
    /// may overtake.
    blocked: bool,
}

/// The largest cone, in nodes, of a set that may overtake. An overtaking run
/// cannot itself be overtaken, so a large set waits its turn and stays
/// passable.
const OVERTAKE_CONE: usize = 64;

/// The most event sets one stint pops between waves.
const STINT_POPS: usize = 4096;

/// The event sets a stint has popped and not settled, in arrival order.
struct Stint {
    held: VecDeque<Held>,
    /// How many more sets the holder may pop between waves. The cap bounds
    /// the overtaking runs one stint settles.
    pops: usize,
    /// Nodes fired by runs that overtook the run in flight.
    overtaken: FxHashSet<usize>,
    scan: Scan,
}

impl Stint {
    /// Take the next run off the front of the held sets.
    fn next_run(&mut self) -> Option<Vec<EventSet>> {
        let len = run_len(
            self.held
                .iter()
                .take_while(|held| held.initial)
                .map(|held| &held.set),
        );
        let run: Vec<EventSet> = self.held.drain(..len).map(|held| held.set).collect();
        self.scan = Scan::default();
        self.overtaken.clear();
        (!run.is_empty()).then_some(run)
    }

    /// Between two waves of the run in flight, settle each held set that
    /// no earlier unsettled set can reach, as a run of its own.
    ///
    /// A set may overtake when it writes something, is not a barrier, shares
    /// no exclusive key with an earlier set, and its cone is known, at most
    /// [`OVERTAKE_CONE`] nodes, and disjoint from the cone of the run in
    /// flight and of every earlier held set. No node then receives its values
    /// ahead of an earlier set's. The sets that may overtake settle in
    /// arrival order, combined into runs by the usual rules.
    fn overtake(
        &mut self,
        flight: &mut InFlight,
        first_panic: &mut Option<Box<dyn Any + Send>>,
        late: &mut Vec<Arc<Waiter>>,
    ) {
        for _ in 0..INPUT.len().min(self.pops) {
            let Some(set) = INPUT.pop() else { break };
            self.pops = self.pops.saturating_sub(1);
            self.held.push_back(Held::new(set, false));
        }
        if flight.barrier || self.scan.blocked || self.held.len() <= self.scan.scanned {
            return;
        }
        let Self {
            held,
            overtaken,
            scan,
            ..
        } = self;
        let Some(reserved) = flight.cone() else {
            scan.blocked = true;
            return;
        };
        let mut ahead: Vec<EventSet> = Vec::new();
        let mut index = scan.scanned;
        while let Some(candidate) = held.get(index) {
            let clash = candidate
                .set
                .keys
                .iter()
                .any(|key| flight.keys.contains(key) || scan.keys.contains(key));
            let open = !candidate.set.barrier && !candidate.set.writes.is_empty() && !clash;
            let cone = if open { candidate.cone() } else { None };
            let disjoint = cone.is_some_and(|cone| {
                cone.len() <= OVERTAKE_CONE
                    && cone
                        .iter()
                        .all(|node| !reserved.contains(node) && !scan.cone.contains(node))
            });
            if open && disjoint {
                let Some(moved) = held.remove(index) else {
                    break;
                };
                ahead.push(moved.set);
                continue;
            }
            if candidate.set.barrier {
                scan.blocked = true;
                break;
            }
            if !candidate.set.writes.is_empty() {
                let Some(cone) = candidate.cone() else {
                    scan.blocked = true;
                    break;
                };
                scan.cone.extend(cone.iter().copied());
            }
            scan.keys.extend(candidate.set.keys.iter().copied());
            index = index.saturating_add(1);
        }
        scan.scanned = index;
        while !ahead.is_empty() {
            let len = run_len(&ahead);
            let rest = ahead.split_off(len);
            late.extend(run_ahead(ahead, overtaken, first_panic));
            ahead = rest;
        }
    }
}

/// Settle `run` ahead of another run that waits between waves.
/// Returns the writers that must wait for the end of that other run, because
/// a value of `set` reached a node the other run holds.
fn run_ahead(
    run: Vec<EventSet>,
    overtaken: &mut FxHashSet<usize>,
    first_panic: &mut Option<Box<dyn Any + Send>>,
) -> Vec<Arc<Waiter>> {
    OVERTAKES.fetch_add(1, Ordering::Relaxed);
    let suspended = ACTIVE.take();
    ACTIVE.set(Some(Active::new(0, None, Arc::default())));
    let mut frontier = Frontier {
        fired: Some(FxHashSet::default()),
        ..Frontier::default()
    };
    let late = settle_run(run, &mut frontier, first_panic, None);
    ACTIVE.set(suspended);
    overtaken.extend(frontier.fired.into_iter().flatten());
    late
}

/// Settle one stint: the event sets queued when the stint began, split into
/// runs and settled in order. Sets popped between waves settle here only by
/// overtaking; the rest wait in [`CARRY`] for the next stint.
///
/// The bound holds whatever the callbacks queue. `INPUT` is one
/// arrival-ordered queue, so settling every callback's set inside the stint
/// would mean settling every other thread's set that arrived before it, and
/// under sustained feedback the stint would never end.
fn settle_queued() -> Option<Box<dyn Any + Send>> {
    let mut first_panic = None;
    let mut frontier = Frontier::default();
    let mut stint = Stint {
        held: VecDeque::new(),
        pops: STINT_POPS,
        overtaken: FxHashSet::default(),
        scan: Scan::default(),
    };
    let carried = std::mem::take(&mut *CARRY.lock());
    stint
        .held
        .extend(carried.into_iter().map(|set| Held::new(set, true)));
    for _ in 0..INPUT.len() {
        let Some(set) = INPUT.pop() else { break };
        stint.held.push_back(Held::new(set, true));
    }
    while let Some(run) = stint.next_run() {
        let late = settle_run(run, &mut frontier, &mut first_panic, Some(&mut stint));
        debug_assert!(late.is_empty(), "a run in order hands no writer on");
    }
    CARRY
        .lock()
        .extend(stint.held.into_iter().map(|held| held.set));
    first_panic
}

/// The writers of a run by tag, and which of them have not woken.
struct Wake {
    groups: Groups,
    live: Tags,
    start: u64,
    fires: u64,
    /// When each tag woke: nanoseconds and fanouts since the run started.
    woke: [Option<(u64, u64)>; TAG_BITS],
}

impl Wake {
    /// Wake the writers of every set whose values no frontier node carries.
    fn drained(&mut self, frontier: &Frontier) {
        let drained = frontier.drained(self.live);
        self.live &= !drained;
        for bit in bits(drained) {
            let elapsed = self.elapsed();
            if let Some(woke) = self.woke.get_mut(bit) {
                *woke = Some(elapsed);
            }
            let group = self.groups.get(bit).map_or(&[][..], Vec::as_slice);
            EARLY_WAKES.fetch_add(
                u64::try_from(group.len()).unwrap_or(u64::MAX),
                Ordering::Relaxed,
            );
            for waiter in group {
                waiter.settled_one(None);
            }
        }
    }

    fn elapsed(&self) -> (u64, u64) {
        (
            now_nanos().saturating_sub(self.start),
            FIRES.load(Ordering::Relaxed).saturating_sub(self.fires),
        )
    }

    /// Count each set's own run: until its writers woke, or to the end.
    fn count_own_runs(&self, classes: Vec<(SetClass, Tags)>) {
        let ended = self.elapsed();
        for (class, tags) in classes {
            let (nanos, fires) = if tags == 0 {
                ended
            } else {
                bits(tags)
                    .next()
                    .and_then(|bit| self.woke.get(bit).copied().flatten())
                    .unwrap_or(ended)
            };
            let counters = class.counters();
            counters.own_run_nanos.fetch_add(nanos, Ordering::Relaxed);
            counters.own_run_fires.fetch_add(fires, Ordering::Relaxed);
        }
    }

    /// The writers of the sets that have not woken, one per set.
    fn remaining(&self) -> Vec<Arc<Waiter>> {
        let mut waiters = Vec::new();
        for bit in bits(self.live) {
            waiters.extend(self.groups.get(bit).into_iter().flatten().cloned());
        }
        waiters
    }
}

/// Settle one run and tell the writers that wait on it.
///
/// A writer wakes as soon as no node in the frontier carries its set's
/// values, which can be long before the run ends. `stint` is `None` for a
/// run that overtook another; such a run returns its writers instead of
/// waking them when one of its values reached a node the other run holds.
///
/// A panic in a callback belongs to the write that caused it. When every set
/// in the run has a waiting writer and one is still asleep at the end of the
/// run, the first panic is re-raised on one of them; otherwise it is
/// re-raised on this thread after the lock is released.
fn settle_run(
    run: Vec<EventSet>,
    frontier: &mut Frontier,
    first_panic: &mut Option<Box<dyn Any + Send>>,
    stint: Option<&mut Stint>,
) -> Vec<Arc<Waiter>> {
    let start = now_nanos();
    count_run_start(&run, start);
    // The frontier is empty between runs; the bits name this run's sets.
    frontier.pinned = 0;
    let all_wait = run.iter().all(|set| !set.waiters.is_empty());
    let mut groups: Vec<Vec<Arc<Waiter>>> = vec![Vec::new(); TAG_BITS];
    let mut set_tags = Vec::with_capacity(run.len());
    let mut waited = 0usize;
    // A set that writes nothing, which `flush` queues, waits for every set
    // before it, so its writers wake only when the run ends.
    let mut at_end: Vec<Arc<Waiter>> = Vec::new();
    for set in &run {
        if set.writes.is_empty() {
            at_end.extend(set.waiters.iter().cloned());
        }
        if set.waiters.is_empty() || set.writes.is_empty() {
            set_tags.push(0);
            continue;
        }
        if let Some(group) = groups.get_mut(waited.min(TAG_BITS - 1)) {
            group.extend(set.waiters.iter().cloned());
        }
        set_tags.push(tag_bit(waited));
        waited = waited.saturating_add(1);
    }
    let classes: Vec<(SetClass, Tags)> = run
        .iter()
        .zip(&set_tags)
        .filter(|(set, _)| !set.writes.is_empty())
        .map(|(set, tags)| (set.class(), *tags))
        .collect();
    let epoch = run.iter().find_map(|set| set.epoch);
    let carriers = run.iter().filter(|set| set.epoch.is_some()).count();
    let mut wake = Wake {
        groups: groups.into(),
        live: set_tags.iter().fold(0, |live, tags| live | tags),
        start,
        fires: FIRES.load(Ordering::Relaxed),
        woke: [None; TAG_BITS],
    };
    ACTIVE.with_borrow_mut(|active| {
        if let Some(active) = active.as_mut() {
            active.epoch = epoch;
            active.groups = Arc::clone(&wake.groups);
            active.tags = 0;
        }
    });
    let overtaker = stint.is_none();
    let mut late = Vec::new();
    let mut panic = None;
    fire_run(
        run, &set_tags, frontier, &mut panic, &mut wake, stint, &mut late,
    );
    if let Some(epoch) = epoch
        && let Some(finished) = release(epoch, u64::try_from(carriers).unwrap_or(u64::MAX))
    {
        ACTIVE.with_borrow_mut(|active| {
            if let Some(active) = active.as_mut() {
                active.epoch = None;
                active.firing = Some(Stamp::AFTER_RUN);
                active.tags = wake.live;
            }
        });
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(finished));
        frontier.merge(take_outbox());
        if let Err(payload) = outcome
            && panic.is_none()
        {
            panic = Some(payload);
        }
    }
    wake.count_own_runs(classes);
    let mut waiters = wake.remaining();
    waiters.append(&mut at_end);
    if overtaker && frontier.escaped() {
        CONE_ESCAPES.fetch_add(1, Ordering::Relaxed);
        if first_panic.is_none() {
            *first_panic = panic;
        }
        return waiters;
    }
    waiters.append(&mut late);
    let mut panic = if (all_wait && !waiters.is_empty()) || first_panic.is_some() {
        panic
    } else {
        *first_panic = panic;
        None
    };
    for waiter in waiters {
        waiter.settled_one(panic.take());
    }
    Vec::new()
}

fn fire_run(
    run: Vec<EventSet>,
    set_tags: &[Tags],
    frontier: &mut Frontier,
    first_panic: &mut Option<Box<dyn Any + Send>>,
    wake: &mut Wake,
    mut stint: Option<&mut Stint>,
    late: &mut Vec<Arc<Waiter>>,
) {
    RUNS.fetch_add(1, Ordering::Relaxed);
    let run_id = RUN_ID.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
    ACTIVE.with_borrow_mut(|active| {
        if let Some(active) = active.as_mut() {
            active.run_id = run_id;
            active.firing = None;
        }
    });
    let mut flight = stint.is_some().then(|| InFlight::new(&run));
    for (set, tags) in run.into_iter().zip(set_tags) {
        ACTIVE.with_borrow_mut(|active| {
            if let Some(active) = active.as_mut() {
                active.tags = *tags;
            }
        });
        for write in set.writes {
            write.apply();
        }
    }
    frontier.merge(take_outbox());
    wake.drained(frontier);
    let mut entries = Vec::new();
    loop {
        if let (Some(stint), Some(flight)) = (stint.as_deref_mut(), flight.as_mut()) {
            stint.overtake(flight, first_panic, late);
        }
        if !frontier.pop_wave(&mut entries) {
            break;
        }
        if let Some(stint) = stint.as_deref()
            && !stint.overtaken.is_empty()
        {
            let escapes = entries
                .iter()
                .filter(|entry| stint.overtaken.contains(&addr(entry.node.as_ptr())))
                .count();
            CONE_ESCAPES.fetch_add(
                u64::try_from(escapes).unwrap_or(u64::MAX),
                Ordering::Relaxed,
            );
        }
        WAVES.fetch_add(1, Ordering::Relaxed);
        let width = u64::try_from(entries.len()).unwrap_or(u64::MAX);
        WIDEST_WAVE.fetch_max(width, Ordering::Relaxed);
        let panic = if let Some(workers) = wave::workers_for(entries.len()) {
            wave::fire(std::mem::take(&mut entries), workers, frontier)
        } else {
            let panic = fire_entries(&entries);
            frontier.merge(take_outbox());
            panic
        };
        if first_panic.is_none() {
            *first_panic = panic;
        }
        wake.drained(frontier);
    }
}

/// Fire `entries` in order on this thread. Returns the first panic.
///
/// What the fires schedule and queue stays in this thread's outbox.
fn fire_entries(entries: &[Entry]) -> Option<Box<dyn Any + Send>> {
    let mut first_panic = None;
    for entry in entries {
        let Some(node) = entry.node.upgrade() else {
            continue;
        };
        // A rewire during this settle can raise a queued node's height. Fire
        // it at its real height so it still follows every dependency.
        let height = node.node_height();
        if height > entry.height {
            request(Request {
                height,
                node: entry.node.clone(),
                raised: true,
                tags: entry.tags,
            });
            continue;
        }
        ACTIVE.with_borrow_mut(|active| {
            if let Some(active) = active.as_mut() {
                active.firing = Some(Stamp {
                    height: entry.height,
                    seq: entry.seq,
                });
                active.tags = entry.tags;
            }
        });
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| node.fire()));
        if let Err(payload) = outcome
            && first_panic.is_none()
        {
            first_panic = Some(payload);
        }
    }
    first_panic
}

/// Run `f` and submit every write it makes on this thread as one event set.
///
/// The writes settle together: no subscriber sees some of them without the
/// rest. A read inside `f` sees the value from before the batch. Nested calls
/// join the outermost batch.
pub fn batch<R>(f: impl FnOnce() -> R) -> R {
    batch_with(&[], None, f)
}

/// [`batch`] with exclusive keys and an epoch.
///
/// The event set never shares a settle with another event set that carries one
/// of `keys`. `epoch` names the write; see [`Epoch`].
pub fn batch_with<R>(keys: &[SettleKey], epoch: Option<Epoch>, f: impl FnOnce() -> R) -> R {
    let (result, set) = collect(keys, epoch, f);
    let submitted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if let Some(set) = set {
            push(set);
        }
    }));
    match (result, submitted) {
        (Ok(value), Ok(())) => value,
        // The caller's own panic is the one it can act on.
        (Err(payload), _) | (Ok(_), Err(payload)) => std::panic::resume_unwind(payload),
    }
}

/// Run `f` with a batch open on this thread. Returns the event set when this
/// call opened the outermost batch and `f` wrote something.
fn collect<R>(
    keys: &[SettleKey],
    epoch: Option<Epoch>,
    f: impl FnOnce() -> R,
) -> (std::thread::Result<R>, Option<EventSet>) {
    BUILDING.with_borrow_mut(|building| {
        let building = building.get_or_insert_with(|| Building {
            depth: 0,
            set: EventSet::new(),
        });
        building.depth = building.depth.saturating_add(1);
        for key in keys {
            building.set.add_key(*key);
        }
        if building.set.epoch.is_none() {
            building.set.epoch = epoch;
            // Counted from here and not from the push: the writer may drop
            // its scope while an outer batch is still collecting this set.
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
    let set = closed.map(|building| building.set);
    let set = match set {
        Some(set) if set.writes.is_empty() => {
            // Never queued, so no settle will release it.
            if let Some(finished) = set.epoch.and_then(|epoch| release(epoch, 1)) {
                finished();
            }
            None
        }
        set => set,
    };
    (result, set)
}

/// [`batch_with`] that never waits.
///
/// A write normally returns once it has settled, which can mean sleeping while
/// another thread settles it. The event set `f` builds is instead queued, and
/// settled on this thread only if no other thread is settling. Use it on a
/// thread that must not stall, such as a network reader or a timer loop. The
/// caller cannot rely on reading its own write back; [`flush`] waits for it.
///
/// When this thread does settle, subscriber callbacks run on it before
/// `submit` returns. Do not call it while holding a lock that a subscriber
/// takes; use [`enqueue`] there.
pub fn submit<R>(keys: &[SettleKey], epoch: Option<Epoch>, f: impl FnOnce() -> R) -> R {
    with_mode(Mode::Submit, || batch_with(keys, epoch, f))
}

/// [`batch_with`] that only queues.
///
/// The event set `f` builds is queued and nothing else happens on this
/// thread: no callback runs and nothing is waited on. Another thread's settle
/// delivers it, or this thread's next write, or [`flush`]. This is the form
/// to use while holding a lock that a subscriber callback takes: queue under
/// the lock, release it, then `flush`.
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
    // A nested call never makes the thread wait more than the outer one asked.
    MODE.set(MODE.get().max(mode));
    f()
}

/// Wait until every event set queued before this call has settled.
///
/// For a thread that used [`submit`] and now needs to read the result, and
/// for code that must release a lock before it can afford to wait.
pub fn flush() {
    let _restore = {
        struct Restore(Mode);
        impl Drop for Restore {
            fn drop(&mut self) {
                MODE.set(self.0);
            }
        }
        let restore = Restore(MODE.get());
        MODE.set(Mode::Wait);
        restore
    };
    FLUSH_SETS.fetch_add(1, Ordering::Relaxed);
    push(EventSet::new());
}

/// Group the writes `f` makes into one event set and queue it, for a
/// mutation that touches several cells of one collection under a lock.
///
/// The caller releases its lock and then calls [`Queued::finish`], which
/// settles the set or waits for it as a plain write would. No callback runs
/// before that.
///
/// During a settle, and inside an open batch, `f` runs as it is and `finish`
/// does nothing: each write of a settle is placed by its own height, so a
/// derived collection that publishes from a callback still joins the settle
/// in progress, and a batch keeps its writes.
///
/// If `f` panics, the writes it made stay queued for a later settle.
pub(crate) fn atomic_queued<R>(f: impl FnOnce() -> R) -> (R, Queued) {
    if ACTIVE.with_borrow(Option::is_some) || BUILDING.with_borrow(Option::is_some) {
        return (f(), Queued::LEFT_QUEUED);
    }
    let (result, set) = collect(&[], None, f);
    let queued = set.map_or(Queued::LEFT_QUEUED, queue);
    match result {
        Ok(value) => (value, queued),
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

/// The node's height, from its cache when the cache is current.
pub(crate) fn height(node: &dyn DepNode) -> u64 {
    let mut stack = FxHashSet::default();
    height_dfs(node, &mut stack)
}

/// Each node's cache packs `(height_epoch << 32) | height` and is valid while
/// the node's own height epoch is unchanged, so an edge change recomputes only
/// the cone it invalidated. `stack` breaks cycles: a back-edge contributes 0.
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
/// depends on it. Called when `start` gains or replaces a dependency.
pub(crate) fn invalidate_height_cone(start: &dyn Node) {
    edges_changed();
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
