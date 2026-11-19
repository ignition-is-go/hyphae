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

struct Entry {
    height: u64,
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

/// The settle in progress on this thread.
struct Active {
    frontier: BinaryHeap<Entry>,
    seq: u64,
    /// Height of the node now firing; `None` while a run is being seeded.
    firing: Option<u64>,
    run_id: u64,
    /// The writers waiting on the run now settling.
    waiters: Vec<Arc<Waiter>>,
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
        Some(firing) if target() > firing => Position::Joins,
        Some(_) => Position::Queue,
    }
}

/// Put `node` in the frontier of the settle in progress on this thread.
pub(crate) fn schedule(node: Weak<dyn Node>, height: u64) {
    ACTIVE.with_borrow_mut(|active| {
        if let Some(active) = active.as_mut() {
            active.seq = active.seq.wrapping_add(1);
            active.frontier.push(Entry {
                height,
                seq: active.seq,
                node,
            });
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

fn push(mut set: EventSet) {
    EVENT_SETS.fetch_add(1, Ordering::Relaxed);
    if ACTIVE.with_borrow(Option::is_some) {
        // Reentrant: this thread holds the lock, so it never waits. The set
        // settles in this stint or a later one. A writer waiting on the
        // settle in progress waits for this set too, so it wakes to the
        // settled result of its write and not to a half-finished chain.
        DEFERRED_WRITES.fetch_add(1, Ordering::Relaxed);
        set.waiters = ACTIVE.with_borrow(|active| {
            active
                .as_ref()
                .map(|active| active.waiters.clone())
                .unwrap_or_default()
        });
        for waiter in &set.waiters {
            waiter.outstanding.fetch_add(1, Ordering::AcqRel);
        }
        INPUT.push(set);
        return;
    }
    let mode = MODE.get();
    if mode == Mode::Enqueue {
        INPUT.push(set);
        return;
    }
    // A stint is bounded, so even a writer that takes the lock can leave with
    // the sets its callbacks queued still unsettled. It waits for those too.
    let waiter = (mode == Mode::Wait).then(Waiter::new);
    set.waiters.extend(waiter.iter().cloned());
    if let Some(guard) = SETTLE.try_lock() {
        INPUT.push(set);
        drain(Some(guard));
    } else {
        LOCK_MISSES.fetch_add(1, Ordering::Relaxed);
        INPUT.push(set);
        // The holder may have released between the failed try and the push,
        // in which case nobody is coming for this set.
        drain(None);
    }
    if let Some(waiter) = waiter {
        // Whoever holds the lock re-checks the queue after releasing it, so
        // some holder settles what this writer waits on and wakes it.
        if let Some(payload) = waiter.wait() {
            std::panic::resume_unwind(payload);
        }
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
            frontier: BinaryHeap::new(),
            seq: 0,
            firing: None,
            run_id: 0,
            waiters: Vec::new(),
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
    let mut run: Vec<EventSet> = Vec::new();
    let mut run_keys: FxHashSet<u64> = FxHashSet::default();
    let mut run_barrier = false;
    for _ in 0..INPUT.len() {
        let Some(set) = INPUT.pop() else { break };
        let conflicts = !run.is_empty()
            && (run_barrier || set.barrier || set.keys.iter().any(|key| run_keys.contains(key)));
        if conflicts {
            settle_run(std::mem::take(&mut run), &mut first_panic);
            run_keys.clear();
            run_barrier = false;
        }
        run_barrier |= set.barrier;
        run_keys.extend(set.keys.iter().copied());
        run.push(set);
    }
    if !run.is_empty() {
        settle_run(run, &mut first_panic);
    }
    first_panic
}

/// Settle one run and tell the writers that wait on it.
///
/// A panic in a callback belongs to the write that caused it. When every set
/// in the run has a waiting writer, the first panic is re-raised on one of
/// them; otherwise it is re-raised on this thread after the lock is released.
fn settle_run(run: Vec<EventSet>, first_panic: &mut Option<Box<dyn Any + Send>>) {
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
            active.waiters = distinct;
        }
    });
    let mut panic = None;
    fire_run(run, &mut panic);
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

fn fire_run(run: Vec<EventSet>, first_panic: &mut Option<Box<dyn Any + Send>>) {
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
    loop {
        let entry = ACTIVE.with_borrow_mut(|active| active.as_mut()?.frontier.pop());
        let Some(entry) = entry else { break };
        let Some(node) = entry.node.upgrade() else {
            continue;
        };
        // A rewire during this settle can raise a queued node's height. Fire
        // it at its real height so it still follows every dependency.
        let height = node.node_height();
        if height > entry.height {
            schedule(entry.node, height);
            continue;
        }
        ACTIVE.with_borrow_mut(|active| {
            if let Some(active) = active.as_mut() {
                active.firing = Some(entry.height);
            }
        });
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| node.fire()));
        if let Err(payload) = outcome
            && first_panic.is_none()
        {
            *first_panic = Some(payload);
        }
    }
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
    let submitted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        if let Some(building) = closed
            && !building.set.writes.is_empty()
        {
            push(building.set);
        }
    }));

    match (result, submitted) {
        (Ok(value), Ok(())) => value,
        // The caller's own panic is the one it can act on.
        (Err(payload), _) | (Ok(_), Err(payload)) => std::panic::resume_unwind(payload),
    }
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

/// Group the writes `f` makes into one event set, for a mutation that touches
/// several cells of one collection.
///
/// Unlike [`batch`], this does nothing during a settle: there each write is
/// placed by its own height, so a derived collection that publishes from a
/// callback still joins the settle in progress.
pub(crate) fn atomic<R>(f: impl FnOnce() -> R) -> R {
    let settling = ACTIVE.with_borrow(Option::is_some) && !BUILDING.with_borrow(Option::is_some);
    if settling { f() } else { batch(f) }
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
