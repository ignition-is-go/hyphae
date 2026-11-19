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
    cell::RefCell,
    cmp::Ordering as CmpOrdering,
    collections::BinaryHeap,
    num::NonZeroU64,
    sync::{
        Arc, Weak,
        atomic::{AtomicU64, Ordering},
    },
};

use crossbeam::queue::SegQueue;
use parking_lot::Mutex;
use rustc_hash::FxHashSet;
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

/// Identifies one write that spans several nodes.
///
/// Under the single settle lock an event set is delivered in one settle, so
/// the epoch changes nothing a subscriber can observe. It is accepted now so
/// that callers do not change when the lock is split; `docs/settle-core.md`
/// says where the tag travels then.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Epoch(NonZeroU64);

static NEXT_EPOCH: AtomicU64 = AtomicU64::new(1);

impl Epoch {
    /// Allocate an epoch that no other call has returned.
    #[must_use]
    pub fn next() -> Self {
        let raw = NEXT_EPOCH.fetch_add(1, Ordering::Relaxed);
        Self(NonZeroU64::new(raw).unwrap_or(NonZeroU64::MIN))
    }
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
}

struct EventSet {
    barrier: bool,
    keys: Vec<u64>,
    /// Not read while one lock settles every event set whole.
    #[allow(dead_code)]
    epoch: Option<Epoch>,
    writes: Vec<Box<dyn Write>>,
    /// The writers blocked until this set has settled: the one that
    /// submitted it, or, for a set a callback wrote during a settle, the
    /// writers that were waiting on that settle.
    waiters: Vec<Arc<Waiter>>,
}

/// Lets a writer that lost the settle lock sleep until the holder has settled
/// its event set.
struct Waiter {
    /// Event sets that must settle before the writer wakes: its own, plus
    /// every set that a callback wrote while one of those was settling.
    outstanding: AtomicU64,
    state: Mutex<WaiterState>,
    settled: parking_lot::Condvar,
}

#[derive(Default)]
struct WaiterState {
    done: bool,
    /// A holder ended its stint with work still queued and asked this
    /// writer to continue the settling.
    drive: bool,
    panic: Option<Box<dyn Any + Send>>,
}

impl Waiter {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            outstanding: AtomicU64::new(1),
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
                drop(state);
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
        panic
    }

    /// Ask this writer to continue the settling. False when it has already
    /// been asked or has finished waiting.
    fn ask_to_drive(&self) -> bool {
        let mut state = self.state.lock();
        if state.done || state.drive {
            return false;
        }
        state.drive = true;
        drop(state);
        self.settled.notify_one();
        true
    }
}

impl EventSet {
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

/// The nodes waiting to fire in the settle in progress. Only the holder
/// touches it.
#[derive(Default)]
struct Frontier {
    heap: BinaryHeap<Entry>,
    seq: u64,
}

impl Frontier {
    fn push(&mut self, height: u64, node: Weak<dyn Node>) {
        self.seq = self.seq.wrapping_add(1);
        self.heap.push(Entry {
            height,
            seq: self.seq,
            node,
        });
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
        true
    }

    /// Take in what a stretch of fires produced. Callers pass outboxes in
    /// the order the fires would have run on one thread, which keeps `seq`
    /// and the input queue independent of thread timing.
    fn merge(&mut self, out: Outbox) {
        for (height, node) in out.scheduled {
            self.push(height, node);
        }
        for set in out.deferred {
            INPUT.push(set);
        }
    }
}

/// What the fires on one thread produced since the outbox was last taken.
#[derive(Default)]
struct Outbox {
    /// Nodes that received a write that joins the settle.
    scheduled: Vec<(u64, Weak<dyn Node>)>,
    /// Event sets that callbacks queued.
    deferred: Vec<EventSet>,
}

/// This thread's part in the settle in progress: the holder's, or that of a
/// pool thread firing part of a wave.
struct Active {
    /// The node now firing; `None` while a run is being seeded.
    firing: Option<Stamp>,
    run_id: u64,
    /// The writers waiting on the run now settling.
    waiters: Arc<[Arc<Waiter>]>,
    out: Outbox,
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

/// Where a write made right now belongs.
pub(crate) enum Position {
    /// It joins the settle in progress on this thread.
    Joins,
    /// It must go through the input queue.
    Queue,
}

/// Decide whether a write to a node of height `target` joins the settle in
/// progress on this thread.
///
/// It joins when a run is being seeded, or when the target is strictly higher
/// than the node now firing, so the target has not fired yet in this settle.
/// A write inside a [`batch`] never joins: the batch keeps its writes together.
pub(crate) fn position(target: impl FnOnce() -> u64) -> Position {
    if BUILDING.with_borrow(Option::is_some) {
        return Position::Queue;
    }
    let Some(firing) = ACTIVE.with_borrow(|active| active.as_ref().map(|active| active.firing))
    else {
        return Position::Queue;
    };
    match firing {
        None => Position::Joins,
        Some(firing) if target() > firing.height => Position::Joins,
        Some(_) => Position::Queue,
    }
}

/// Put `node` in the frontier of the settle in progress on this thread.
pub(crate) fn schedule(node: Weak<dyn Node>, height: u64) {
    ACTIVE.with_borrow_mut(|active| {
        if let Some(active) = active.as_mut() {
            active.out.scheduled.push((height, node));
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
    let mut set = EventSet {
        barrier: false,
        keys: Vec::new(),
        epoch: None,
        writes: vec![write],
        waiters: Vec::new(),
    };
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
    if ACTIVE.with_borrow(Option::is_some) {
        // Reentrant: this thread is firing a node, so it never waits. The
        // set reaches the input queue when the holder merges this thread's
        // outbox, and a later stint settles it. A writer waiting on the
        // settle in progress waits for this set too, so it wakes to the
        // settled result of its write and not to a half-finished chain.
        DEFERRED_WRITES.fetch_add(1, Ordering::Relaxed);
        ACTIVE.with_borrow_mut(|active| {
            if let Some(active) = active.as_mut() {
                set.waiters = active.waiters.to_vec();
                for waiter in &set.waiters {
                    waiter.outstanding.fetch_add(1, Ordering::AcqRel);
                }
                active.out.deferred.push(set);
            }
        });
        return Queued::LEFT_QUEUED;
    }
    let mode = MODE.get();
    // A stint is bounded, so even a writer that takes the lock can leave with
    // the sets its callbacks queued still unsettled. It waits for those too.
    let waiter = (mode == Mode::Wait).then(Waiter::new);
    set.waiters.extend(waiter.iter().cloned());
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
        ACTIVE.set(Some(Active {
            firing: None,
            run_id: 0,
            waiters: Arc::default(),
            out: Outbox::default(),
        }));
        let panic = settle_queued();
        ACTIVE.set(None);
        let nanos = u64::try_from(started.elapsed().as_nanos()).unwrap_or(u64::MAX);
        SETTLE_NANOS.fetch_add(nanos, Ordering::Relaxed);
        MAX_HOLD_NANOS.fetch_max(nanos, Ordering::Relaxed);
        drop(held);
        if first_panic.is_none() {
            first_panic = panic;
        }
        // A stint is bounded, and a writer may have pushed after the last pop
        // and missed the lock before the release above. Either way nobody
        // else is coming for what is queued, unless a sleeping writer takes
        // over. Handing over keeps one thread from settling for everyone.
        if INPUT.is_empty() {
            break;
        }
        let handed_over = {
            let waiting = WAITING.lock();
            waiting.iter().any(|waiter| waiter.ask_to_drive())
        };
        if handed_over {
            HANDOVERS.fetch_add(1, Ordering::Relaxed);
            break;
        }
        std::thread::yield_now();
    }
    if let Some(payload) = first_panic {
        std::panic::resume_unwind(payload);
    }
}

/// Settle one stint: the event sets queued when the stint began, split into
/// runs and settled in order.
///
/// The bound holds whatever the callbacks queue. `INPUT` is one arrival-ordered
/// queue, so settling a callback's set inside the stint would mean settling
/// every other thread's set that arrived before it, and under sustained
/// feedback the stint would never end.
fn settle_queued() -> Option<Box<dyn Any + Send>> {
    let mut first_panic = None;
    let mut frontier = Frontier::default();
    let mut run: Vec<EventSet> = Vec::new();
    let mut run_keys: FxHashSet<u64> = FxHashSet::default();
    let mut run_barrier = false;
    for _ in 0..INPUT.len() {
        let Some(set) = INPUT.pop() else { break };
        let conflicts = !run.is_empty()
            && (run_barrier || set.barrier || set.keys.iter().any(|key| run_keys.contains(key)));
        if conflicts {
            settle_run(std::mem::take(&mut run), &mut frontier, &mut first_panic);
            run_keys.clear();
            run_barrier = false;
        }
        run_barrier |= set.barrier;
        run_keys.extend(set.keys.iter().copied());
        run.push(set);
    }
    if !run.is_empty() {
        settle_run(run, &mut frontier, &mut first_panic);
    }
    first_panic
}

/// Settle one run and tell the writers that wait on it.
///
/// A panic in a callback belongs to the write that caused it. When every set
/// in the run has a waiting writer, the first panic is re-raised on one of
/// them; otherwise it is re-raised on this thread after the lock is released.
fn settle_run(
    run: Vec<EventSet>,
    frontier: &mut Frontier,
    first_panic: &mut Option<Box<dyn Any + Send>>,
) {
    let all_wait = run.iter().all(|set| !set.waiters.is_empty());
    let waiters: Vec<Arc<Waiter>> = run
        .iter()
        .flat_map(|set| set.waiters.iter().cloned())
        .collect();
    // A set that a callback queues waits on each writer of this run once,
    // however many sets of the run that writer waits on. Without this a
    // feedback chain multiplies the list at every link.
    let mut distinct: Vec<Arc<Waiter>> = Vec::new();
    for waiter in &waiters {
        if !distinct.iter().any(|seen| Arc::ptr_eq(seen, waiter)) {
            distinct.push(waiter.clone());
        }
    }
    ACTIVE.with_borrow_mut(|active| {
        if let Some(active) = active.as_mut() {
            active.waiters = distinct.into();
        }
    });
    let mut panic = None;
    fire_run(run, frontier, &mut panic);
    let mut panic = if all_wait || first_panic.is_some() {
        panic
    } else {
        *first_panic = panic;
        None
    };
    for waiter in waiters {
        waiter.settled_one(panic.take());
    }
}

fn fire_run(
    run: Vec<EventSet>,
    frontier: &mut Frontier,
    first_panic: &mut Option<Box<dyn Any + Send>>,
) {
    RUNS.fetch_add(1, Ordering::Relaxed);
    let run_id = RUN_ID.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
    ACTIVE.with_borrow_mut(|active| {
        if let Some(active) = active.as_mut() {
            active.run_id = run_id;
            active.firing = None;
        }
    });
    for set in run {
        for write in set.writes {
            write.apply();
        }
    }
    frontier.merge(take_outbox());
    let mut entries = Vec::new();
    while frontier.pop_wave(&mut entries) {
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
            schedule(entry.node.clone(), height);
            continue;
        }
        ACTIVE.with_borrow_mut(|active| {
            if let Some(active) = active.as_mut() {
                active.firing = Some(Stamp {
                    height: entry.height,
                    seq: entry.seq,
                });
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
            set: EventSet {
                barrier: false,
                keys: Vec::new(),
                epoch: None,
                writes: Vec::new(),
                waiters: Vec::new(),
            },
        });
        building.depth = building.depth.saturating_add(1);
        for key in keys {
            building.set.add_key(*key);
        }
        if building.set.epoch.is_none() {
            building.set.epoch = epoch;
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
    let set = closed
        .map(|building| building.set)
        .filter(|set| !set.writes.is_empty());
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
    push(EventSet {
        barrier: false,
        keys: Vec::new(),
        epoch: None,
        writes: Vec::new(),
        waiters: Vec::new(),
    });
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
