//! Write order for a store that keeps its own rows beside a diff cell.
//!
//! A `CellMap` or `CellSet` puts a row in its data when the write is made and
//! tells its subscribers through cells. The two steps are not one: a write
//! made inside a batch reaches the tick when the batch closes, and a write
//! made outside one is delivered after its row is stored. Left alone, a later
//! row could be delivered before an earlier one, and everything derived from
//! the diffs would end on a row the store no longer holds.
//!
//! [`WriteOrder`] closes that gap for one store.
//!
//! - Rows are stored under its lock, so the rows of one store have one order.
//! - A write holds the keys it stored until its delivery is in the tick or
//!   has run: a write made inside a batch until the batch commits, a write
//!   made outside one until its cells are set.
//! - A write to a key a batch holds joins that batch's **commit group**. A
//!   group enters the tick in one step, when the last of its members
//!   finishes, in the order the rows were stored. No member waits for
//!   another: a batch that closes while its group is still open leaves its
//!   writes with the group and returns.
//! - A write made outside a batch, to a key another thread is delivering
//!   outside a batch, is handed to that thread, which delivers it once its
//!   own delivery has returned. The writer does not wait, and the tick is
//!   not involved.
//!
//! So for every key the diffs are delivered in the order the rows were
//! stored, a batch stays atomic, and two batches that write the same keys in
//! opposite orders commit together instead of waiting on each other. Writes
//! that share no key are delivered independently, as they are for cells: two
//! threads that write different keys outside a batch run their subscribers
//! at the same time. [`Sequenced`] keeps a cell that describes the whole
//! store in order across such deliveries.
//!
//! The lock is held while rows are stored, which can run a write hook, and
//! never while subscribers run.
//!
//! Most writes meet nobody: one thread delivers a write, or one batch holds
//! the store, and no other writer holds anything here. That writer is kept
//! apart as the store's **sole holder**, beside the list of holders, and
//! gives its keys back with one atomic exchange instead of the lock. Any
//! other writer that finds a sole holder first moves it into the list, under
//! the lock, and from then on both take the general path. A sole holder
//! never has followers or a group, since giving it one moves it to the list
//! first.

#![allow(clippy::redundant_pub_crate)]

use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
};

use parking_lot::ReentrantMutex;
use rustc_hash::FxHashMap;

use super::{Staged, TickRef, domain::DomainInner};

/// One member of a commit group: a batch, or a thread's deliveries of
/// unbatched writes to one store that a batch has joined.
pub(crate) struct Writer {
    /// Set under the tick lock in the step that puts a batch's writes in the
    /// tick. The keys a committed batch still holds are free.
    committed: AtomicBool,
    /// The commit group this writer belongs to, 0 for none. Written under
    /// the tick lock.
    group: AtomicU64,
}

thread_local! {
    /// The writer of this thread's last committed batch, once nothing else
    /// refers to it, kept so the next batch does not allocate one.
    static SPARE_WRITER: Cell<Option<Arc<Writer>>> = const { Cell::new(None) };
}

impl Writer {
    fn new() -> Arc<Self> {
        SPARE_WRITER.take().unwrap_or_else(|| {
            Arc::new(Self {
                committed: AtomicBool::new(false),
                group: AtomicU64::new(0),
            })
        })
    }

    /// Keep `writer` for this thread's next batch if its claims are all
    /// released and no store or group refers to it any more.
    pub(super) fn retire(mut writer: Arc<Self>) {
        if let Some(sole) = Arc::get_mut(&mut writer) {
            *sole.committed.get_mut() = false;
            *sole.group.get_mut() = 0;
            SPARE_WRITER.set(Some(writer));
        }
    }

    fn is_committed(&self) -> bool {
        self.committed.load(Ordering::Acquire)
    }

    fn group(&self) -> u64 {
        self.group.load(Ordering::Acquire)
    }

    /// The number that names this writer as a holder: its address, which
    /// no thread's [`thread_id`] and no other live writer shares. Exposed,
    /// so [`WriteOrder::share`] can reach the writer from it.
    fn token(writer: &Arc<Self>) -> usize {
        Arc::as_ptr(writer).expose_provenance()
    }
}

/// One store a batch holds keys pending on.
pub(super) struct Claim {
    order: Arc<WriteOrder>,
    /// The batch, kept alive while it may be the store's sole holder.
    writer: Arc<Writer>,
}

impl Claim {
    /// Free the keys. Called once the batch's writes are in the tick, with
    /// no tick lock held.
    pub(super) fn release(self) {
        if self.order.leave_solo(Writer::token(&self.writer)) {
            return;
        }
        let released = self.order.with_pending(|pending| {
            pending
                .held
                .iter()
                .position(|held| held.is(&self.writer))
                .map(|index| pending.held.swap_remove(index))
        });
        if let Some(held) = released {
            held.recycle();
        }
    }
}

thread_local! {
    /// The key list of the last hold this thread released, kept so the next
    /// one does not allocate.
    static SPARE_KEYS: Cell<Vec<(u64, u64)>> = const { Cell::new(Vec::new()) };
}

/// The keys one writer holds on one store.
struct Held {
    /// The batch that holds them. For a thread's unbatched deliveries, the
    /// member a batch that joined them made for them, if one did.
    writer: Option<Arc<Writer>>,
    /// The thread delivering unbatched writes, 0 for a batch.
    thread: usize,
    keys: Keys,
    /// The writer stored every key at once.
    whole: bool,
    /// How many unbatched writes the thread is delivering. A subscriber that
    /// writes the store adds one.
    deliveries: u32,
    /// Stamp of the batch's last row here.
    last: u64,
    /// Stamp of the first row of the run of adjacent rows that ends at
    /// `last`.
    run: u64,
    /// Stamp of the batch's last row here that stored every key.
    whole_at: u64,
    /// Writes other threads made to these keys meanwhile, which the thread
    /// delivers, in order, when its own delivery has returned.
    followers: VecDeque<Follower>,
}

/// A stored write whose delivery waits for another thread's.
type Follower = Box<dyn FnOnce() + Send>;

impl Held {
    fn new(writer: Option<Arc<Writer>>, thread: usize) -> Self {
        Self {
            writer,
            thread,
            keys: Keys {
                first: None,
                few: Vec::new(),
                many: FxHashMap::default(),
            },
            whole: false,
            deliveries: 0,
            last: 0,
            run: 0,
            whole_at: 0,
            followers: VecDeque::new(),
        }
    }

    fn is(&self, writer: &Arc<Writer>) -> bool {
        self.token() == Some(Writer::token(writer))
    }

    fn token(&self) -> Option<usize> {
        self.writer.as_ref().map(Writer::token)
    }

    /// Forget the keys of an earlier holder, keeping the list's memory.
    fn reset(&mut self, thread: usize) {
        self.thread = thread;
        self.keys.first = None;
        self.keys.few.clear();
        self.keys.many.clear();
        self.whole = false;
        self.deliveries = 0;
        self.last = 0;
        self.run = 0;
        self.whole_at = 0;
    }

    /// Hold the keys for a row stamped `stamp` (0 outside a batch), and
    /// return the stamp of this holder's latest earlier row to any of them.
    fn hold(&mut self, touched: &Touched, stamp: u64) -> u64 {
        let latest = match touched {
            Touched::Key(hash) => self.keys.insert(*hash, stamp),
            Touched::Keys(hashes) => {
                // Taken over every key before any is stamped, so a key the
                // row names twice does not count as written before it.
                let latest = hashes.iter().map(|hash| self.keys.get(*hash)).max();
                for hash in hashes {
                    self.keys.insert(*hash, stamp);
                }
                latest.unwrap_or(0)
            }
            Touched::All => {
                self.whole = true;
                let latest = self.last;
                self.whole_at = stamp;
                return latest;
            }
        };
        latest.max(self.whole_at)
    }

    /// Keep the key list, if one was needed, for this thread's next hold.
    fn recycle(self) {
        let mut few = self.keys.few;
        if few.capacity() > 0 {
            few.clear();
            SPARE_KEYS.set(few);
        }
    }
}

/// Key hashes, each with the stamp of its latest row: one inline, a list
/// while it is short, indexed once it is not.
struct Keys {
    first: Option<(u64, u64)>,
    few: Vec<(u64, u64)>,
    many: FxHashMap<u64, u64>,
}

/// Longest key list searched linearly.
const KEYS_SCAN: usize = 16;

impl Keys {
    /// Stamp `hash` and return its previous stamp, 0 when it was not held.
    #[inline]
    fn insert(&mut self, hash: u64, stamp: u64) -> u64 {
        match &mut self.first {
            None if self.few.is_empty() && self.many.is_empty() => {
                self.first = Some((hash, stamp));
                return 0;
            }
            Some(first) if first.0 == hash => return std::mem::replace(&mut first.1, stamp),
            _ => {}
        }
        let slot = match &mut self.first {
            Some(first) if first.0 == hash => Some(&mut first.1),
            _ => self
                .few
                .iter_mut()
                .find(|(held, _)| *held == hash)
                .map(|(_, at)| at)
                .or_else(|| self.many.get_mut(&hash)),
        };
        if let Some(at) = slot {
            return std::mem::replace(at, stamp);
        }
        if self.first.is_none() {
            self.first = Some((hash, stamp));
        } else if !self.many.is_empty() {
            self.many.insert(hash, stamp);
        } else if self.few.len() < KEYS_SCAN {
            if self.few.capacity() == 0 {
                self.few = SPARE_KEYS.take();
            }
            self.few.push((hash, stamp));
        } else {
            self.many.extend(self.few.drain(..));
            self.many.insert(hash, stamp);
        }
        0
    }

    /// The stamp of `hash`, 0 when it is not held.
    fn get(&self, hash: u64) -> u64 {
        match self.first {
            Some((first, at)) if first == hash => at,
            _ => self
                .few
                .iter()
                .find(|(held, _)| *held == hash)
                .map(|(_, at)| *at)
                .or_else(|| self.many.get(&hash).copied())
                .unwrap_or(0),
        }
    }

    fn contains(&self, hash: u64) -> bool {
        self.first.is_some_and(|(first, _)| first == hash)
            || self.few.iter().any(|(held, _)| *held == hash)
            || self.many.contains_key(&hash)
    }
}

/// Writers that commit together, because committed apart they would
/// deliver one key out of stored order.
struct Group {
    /// Members whose batch is open or whose delivery is running.
    open: u32,
    members: Vec<Arc<Writer>>,
    /// Writes of members that closed while the group was open.
    parked: Vec<Staged>,
    claims: Vec<Claim>,
}

/// One tick's commit groups. Lives under the tick lock.
#[derive(Default)]
pub(super) struct Groups {
    next: u64,
    live: FxHashMap<u64, Group>,
}

impl Groups {
    /// Put two unfinished writers in one group.
    fn join(&mut self, one: &Arc<Writer>, other: &Arc<Writer>) {
        let (kept, absorbed) = match (one.group(), other.group()) {
            (0, 0) => {
                self.next = self.next.saturating_add(1);
                self.live.insert(
                    self.next,
                    Group {
                        open: 0,
                        members: Vec::new(),
                        parked: Vec::new(),
                        claims: Vec::new(),
                    },
                );
                self.admit(self.next, one);
                self.admit(self.next, other);
                return;
            }
            (group, 0) => return self.admit(group, other),
            (0, group) => return self.admit(group, one),
            (kept, absorbed) if kept == absorbed => return,
            pair => pair,
        };
        let Some(mut absorbed) = self.live.remove(&absorbed) else {
            return;
        };
        let Some(group) = self.live.get_mut(&kept) else {
            return;
        };
        for member in &absorbed.members {
            member.group.store(kept, Ordering::Release);
        }
        group.open = group.open.saturating_add(absorbed.open);
        group.members.append(&mut absorbed.members);
        group.parked.append(&mut absorbed.parked);
        group.claims.append(&mut absorbed.claims);
    }

    /// Add `writer`, which belongs to no group and has not finished.
    fn admit(&mut self, group: u64, writer: &Arc<Writer>) {
        if let Some(members) = self.live.get_mut(&group) {
            members.open = members.open.saturating_add(1);
            members.members.push(Arc::clone(writer));
            writer.group.store(group, Ordering::Release);
        }
    }

    /// `writer` finished. A batch closed with `staged` and `claims`. A
    /// thread's unbatched deliveries all returned, and it brings nothing.
    ///
    /// On return `staged` holds what enters the tick now, in order, and
    /// `claims` what to release afterwards: the batch's own for a batch in
    /// no group, everything the group held when this was its last open
    /// member, and nothing while another member is still open.
    pub(super) fn finish(
        &mut self,
        writer: &Arc<Writer>,
        batch: bool,
        staged: &mut Vec<Staged>,
        claims: &mut Vec<Claim>,
    ) {
        let id = writer.group();
        let Some(group) = (id != 0).then(|| self.live.get_mut(&id)).flatten() else {
            if batch {
                writer.committed.store(true, Ordering::Release);
            }
            return;
        };
        group.open = group.open.saturating_sub(1);
        if !batch {
            // The deliveries are over, and their member is dropped.
            group.members.retain(|member| !Arc::ptr_eq(member, writer));
            writer.group.store(0, Ordering::Release);
        }
        if group.open > 0 {
            group.parked.append(staged);
            group.claims.append(claims);
            return;
        }
        let Some(mut group) = self.live.remove(&id) else {
            return;
        };
        for member in &group.members {
            member.committed.store(true, Ordering::Release);
            member.group.store(0, Ordering::Release);
        }
        if !group.parked.is_empty() {
            group.parked.append(staged);
            // Stable, so each member's writes to one cell keep their order.
            group.parked.sort_by_key(|op| op.stamp);
            std::mem::swap(staged, &mut group.parked);
        }
        claims.append(&mut group.claims);
    }
}

impl Default for Held {
    fn default() -> Self {
        Self::new(None, 0)
    }
}

#[derive(Default)]
struct Pending {
    /// The writers with rows stored here and not yet delivered or in the
    /// tick, other than the sole holder.
    held: Vec<Held>,
    /// The sole holder's keys, current while [`WriteOrder::solo`] names one.
    /// Its `writer` stays `None`: [`WriteOrder::share`] fills it in.
    solo: Held,
    /// Stamp of the last row a batch stored here.
    clock: u64,
}

/// The hash a store names a key by in [`Touched`].
pub(crate) fn key_hash<K: std::hash::Hash>(key: &K) -> u64 {
    use std::hash::BuildHasher;
    rustc_hash::FxBuildHasher.hash_one(key)
}

/// What one write stored.
pub(crate) enum Touched {
    /// The [`key_hash`] of the one key. Two keys with one hash are treated
    /// as one key, which only orders two writes that did not need it.
    Key(u64),
    Keys(Vec<u64>),
    /// Every key the store holds or held.
    All,
}

impl Touched {
    fn overlaps(&self, held: &Held) -> bool {
        held.whole
            || match self {
                Self::Key(hash) => held.keys.contains(*hash),
                Self::Keys(hashes) => hashes.iter().any(|hash| held.keys.contains(*hash)),
                Self::All => true,
            }
    }
}

/// The write order of one store. See the [module docs](self).
#[derive(Default)]
pub(crate) struct WriteOrder {
    pending: ReentrantMutex<RefCell<Pending>>,
    /// The sole holder: a delivering thread's [`thread_id`] or a batch's
    /// [`Writer::token`], 0 for none. Set under the lock. Cleared by the
    /// holder without the lock when it lets go, or under the lock by a
    /// writer that moves it into the list. Whichever clears it first wins,
    /// so a holder that loses knows it was moved and takes the general path.
    ///
    /// A batch's token stays set only while the [`Claim`] that keeps its
    /// writer alive is unreleased: the claim clears it before letting go.
    solo: AtomicUsize,
}

impl WriteOrder {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Make the holder `token` the sole holder, `thread` for a delivery and
    /// 0 for a batch. Called under the lock, with no holder in the list and
    /// none sole.
    #[inline]
    fn enter_solo(&self, pending: &mut Pending, token: usize, thread: usize) {
        pending.solo.reset(thread);
        self.solo.store(token, Ordering::Release);
    }

    /// Let go as the sole holder `token`, without the lock. False when it
    /// is not sole any more: its keys were moved into the list.
    #[inline]
    fn leave_solo(&self, token: usize) -> bool {
        self.solo
            .compare_exchange(token, 0, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// Move the sole holder, if there is one, into the list. Called under
    /// the lock by any writer before it looks at the list.
    fn share(&self, pending: &mut Pending) {
        let token = self.solo.load(Ordering::Acquire);
        if token == 0
            || self
                .solo
                .compare_exchange(token, 0, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            return;
        }
        let mut held = std::mem::take(&mut pending.solo);
        if held.thread == 0 {
            let writer = std::ptr::with_exposed_provenance::<Writer>(token);
            // SAFETY: `token` is the address of a batch's writer, exposed by
            // `Writer::token`. The batch's unreleased claim holds an `Arc`
            // of it: the claim clears the token before it drops that `Arc`,
            // and the exchange above cleared it first, so the claim has not
            // let go and cannot until this thread releases the lock.
            held.writer = Some(unsafe {
                Arc::increment_strong_count(writer);
                Arc::from_raw(writer)
            });
        }
        pending.held.push(held);
    }

    fn with_pending<R>(&self, f: impl FnOnce(&mut Pending) -> R) -> R {
        f(&mut self.pending.lock().borrow_mut())
    }

    /// Run one write of a store of `domain`.
    ///
    /// `touched` names the keys `store` may change. The caller hashes them
    /// before the call, so a key's `Hash` never runs under the store's lock.
    /// `store` changes the rows, in the store's order, and returns what the
    /// store's cells must be told, or `None` when nothing changed. The rest
    /// is [`Locked::deliver`].
    #[inline]
    pub(crate) fn write<R, E: Send + 'static>(
        self: &Arc<Self>,
        domain: Option<&Arc<DomainInner>>,
        touched: Touched,
        store: impl FnOnce() -> (R, Option<E>),
        whole: impl FnOnce(&E) -> bool,
        emit: impl FnOnce(E, Staging),
        follow: impl FnOnce() -> Box<dyn FnOnce(E) + Send>,
    ) -> R {
        let locked = self.lock();
        let (result, emission) = store();
        if let Some(emission) = emission {
            locked.deliver(domain, touched, emission, whole, emit, follow);
        }
        result
    }

    /// Take the store's lock to store a write's rows under it.
    #[inline]
    pub(crate) fn lock(self: &Arc<Self>) -> Locked<'_> {
        Locked {
            order: self,
            guard: self.pending.lock(),
        }
    }

    /// Choose the route of a write made outside a batch, and hold its keys
    /// when this thread delivers it now.
    fn route<'a>(
        self: &'a Arc<Self>,
        pending: &mut Pending,
        domain: Option<&'a Arc<DomainInner>>,
        touched: &Touched,
    ) -> Route<'a> {
        let thread = thread_id();
        self.share(pending);
        if pending.held.is_empty() {
            self.enter_solo(pending, thread, thread);
            pending.solo.hold(touched, 0);
            pending.solo.deliveries = 1;
            return Route::Deliver(Delivery {
                order: self,
                domain,
                thread,
                solo: true,
            });
        }
        let mut mine = None;
        let mut holder = None;
        for (index, held) in pending.held.iter().enumerate() {
            if held.thread == thread {
                mine = Some(index);
            } else if touched.overlaps(held) {
                // A committed batch counts until it lets go of the key: its
                // writes may not have reached the tick yet, and a delivery
                // on this thread would not look there before it runs.
                if held.thread == 0 || holder.is_some() {
                    return Route::Stage;
                }
                holder = Some(index);
            }
        }
        if let Some(holder) = holder {
            return Route::Follow(holder);
        }
        let mine = mine.unwrap_or_else(|| {
            pending.held.push(Held::new(None, thread));
            pending.held.len().saturating_sub(1)
        });
        if let Some(held) = pending.held.get_mut(mine) {
            held.hold(touched, 0);
            held.deliveries = held.deliveries.saturating_add(1);
        }
        Route::Deliver(Delivery {
            order: self,
            domain,
            thread,
            solo: false,
        })
    }

    /// Join every unfinished writer that holds one of the keys, and hold the
    /// keys for `batch`. Returns the stamp of the row this batch stored
    /// here before, when this row may join its delivery (0 otherwise), and
    /// the stamp of this one.
    ///
    /// A row may join when no other row was stored here since and it names
    /// no key the run of rows it would join already names, so a merged
    /// delivery never carries one key twice unless one write's own diff
    /// did: each write to a key stays its own emission, in stored order.
    /// Two keys with one hash count as one, which only splits a delivery.
    fn hold(
        self: &Arc<Self>,
        pending: &mut Pending,
        domain: Option<&Arc<DomainInner>>,
        batch: &mut super::OpenBatch,
        touched: &Touched,
    ) -> (u64, u64) {
        let writer = &*batch.writer.get_or_insert_with(Writer::new);
        let token = Writer::token(writer);
        if self.solo.load(Ordering::Acquire) == token {
            return Self::stamp(pending, None, batch, touched);
        }
        self.share(pending);
        if pending.held.is_empty() {
            self.enter_solo(pending, token, 0);
            batch.claims.push(Claim {
                order: Arc::clone(self),
                writer: Arc::clone(writer),
            });
            return Self::stamp(pending, None, batch, touched);
        }
        let thread = thread_id();
        let mut mine = None;
        for (index, held) in pending.held.iter_mut().enumerate() {
            if held.is(writer) {
                mine = Some(index);
                continue;
            }
            // A delivery this thread is running is one the batch is nested
            // in. Both run on this thread, in order.
            if held.thread == thread || !touched.overlaps(held) {
                continue;
            }
            // A thread's deliveries become a member when a batch first
            // follows them.
            let holder = if held.thread == 0 {
                held.writer.as_ref()
            } else {
                Some(&*held.writer.get_or_insert_with(Writer::new))
            };
            if let Some(holder) = holder
                && !holder.is_committed()
            {
                let mut tick = TickRef::of(domain).queue.lock();
                // Committed while this thread waited for the tick.
                if !holder.is_committed() {
                    tick.groups.join(writer, holder);
                }
            }
        }
        let mine = mine.unwrap_or_else(|| {
            batch.claims.push(Claim {
                order: Arc::clone(self),
                writer: Arc::clone(writer),
            });
            pending.held.push(Held::new(Some(Arc::clone(writer)), 0));
            pending.held.len().saturating_sub(1)
        });
        Self::stamp(pending, Some(mine), batch, touched)
    }

    /// Stamp the batch's row and hold its keys in its entry: the sole
    /// holder for `None`, else that index of the list. Returns what
    /// [`hold`](Self::hold) does.
    fn stamp(
        pending: &mut Pending,
        mine: Option<usize>,
        batch: &mut super::OpenBatch,
        touched: &Touched,
    ) -> (u64, u64) {
        // Later than every row stored here before and than everything this
        // batch wrote before.
        let stamp = batch.stamp.max(pending.clock).saturating_add(1);
        let clock = pending.clock;
        let held = match mine {
            None => Some(&mut pending.solo),
            Some(index) => pending.held.get_mut(index),
        };
        let mut previous = 0;
        if let Some(held) = held {
            let adjacent = held.last != 0 && held.last == clock;
            let repeats = held.hold(touched, stamp) >= held.run;
            if adjacent && !repeats {
                previous = held.last;
            } else {
                held.run = stamp;
            }
            held.last = stamp;
        }
        pending.clock = stamp;
        batch.stamp = stamp;
        (previous, stamp)
    }
}

/// A store's lock, held by a write while it stores its rows.
pub(crate) struct Locked<'a> {
    order: &'a Arc<WriteOrder>,
    guard: parking_lot::ReentrantMutexGuard<'a, RefCell<Pending>>,
}

impl<'a> Locked<'a> {
    /// The write stored a row of the one key `hash`. When this thread holds
    /// no batch of `domain` and no other writer holds anything here, the
    /// thread becomes the store's sole holder and delivers the write now,
    /// before it drops the returned delivery. Otherwise the lock comes back
    /// for [`deliver`](Self::deliver).
    #[inline]
    pub(crate) fn alone(
        self,
        domain: Option<&'a Arc<DomainInner>>,
        hash: u64,
    ) -> Result<Delivery<'a>, Self> {
        let order = self.order;
        if order.solo.load(Ordering::Acquire) != 0
            || super::holds_open_batch(super::domain::id_of(domain))
        {
            return Err(self);
        }
        let thread = thread_id();
        {
            let mut pending = self.guard.borrow_mut();
            if !pending.held.is_empty() {
                drop(pending);
                return Err(self);
            }
            order.enter_solo(&mut pending, thread, thread);
            pending.solo.keys.first = Some((hash, 0));
            pending.solo.deliveries = 1;
        }
        drop(self.guard);
        Ok(Delivery {
            order,
            domain,
            thread,
            solo: true,
        })
    }

    /// Deliver what the write stored. `whole` says whether `emission` speaks
    /// for every key, whatever keys `touched` named. `emit` then sets the
    /// cells, on the route that delivers them in that order (see the
    /// [module docs](self)). When the write has to follow another thread's
    /// delivery, `follow` gives an `emit` that thread can run.
    pub(crate) fn deliver<E: Send + 'static>(
        self,
        domain: Option<&Arc<DomainInner>>,
        touched: Touched,
        emission: E,
        whole: impl FnOnce(&E) -> bool,
        emit: impl FnOnce(E, Staging),
        follow: impl FnOnce() -> Box<dyn FnOnce(E) + Send>,
    ) {
        let Self {
            order: this,
            guard: order,
        } = self;
        let id = super::domain::id_of(domain);
        let touched = if whole(&emission) {
            Touched::All
        } else {
            touched
        };
        let hold = |batch: &mut super::OpenBatch| {
            this.hold(&mut order.borrow_mut(), domain, batch, &touched)
        };
        if let Some((previous, stamp)) = super::with_open_batch(id, hold) {
            // Staged in the thread's open batch.
            drop(order);
            emit(emission, Staging::Batched { previous, stamp });
            return;
        }
        let route = this.route(&mut order.borrow_mut(), domain, &touched);
        match route {
            Route::Deliver(delivery) => {
                // Set on this thread or queued on the tick, as each cell
                // decides.
                drop(order);
                emit(emission, Staging::Unbatched);
                drop(delivery);
            }
            Route::Follow(holder) => {
                // Another thread is delivering a write to one of the keys.
                // It delivers this one when it has finished.
                let emit = follow();
                if let Some(held) = order.borrow_mut().held.get_mut(holder) {
                    held.hold(&touched, 0);
                    held.followers.push_back(Box::new(move || emit(emission)));
                }
                drop(order);
            }
            Route::Stage => {
                // A batch holds one of the keys, so the write is staged in a
                // batch opened for it alone and joins the holder.
                super::begin_batch(domain);
                super::with_open_batch(id, hold);
                drop(order);
                let batch = Implicit(domain);
                emit(emission, Staging::Unbatched);
                drop(batch);
            }
        }
    }
}

/// A number no other live thread has, and never 0.
#[inline]
fn thread_id() -> usize {
    thread_local! {
        static MARK: u8 = const { 0 };
    }
    MARK.with(|mark| std::ptr::from_ref(mark).addr())
}

/// How [`WriteOrder::write`] hands a write's emission over.
#[derive(Clone, Copy)]
pub(crate) enum Staging {
    /// Delivered alone: now, after another thread's delivery, or through a
    /// batch opened for it.
    Unbatched,
    /// Staged in the writer's open batch, for the row stamped `stamp`.
    /// `previous` is the stamp of the batch's last row in the store when the
    /// two emissions may be delivered as one (see [`WriteOrder::hold`]); 0
    /// otherwise.
    Batched { previous: u64, stamp: u64 },
}

/// How a write made outside a batch is delivered.
enum Route<'a> {
    /// By this thread, now.
    Deliver(Delivery<'a>),
    /// By the thread whose delivery holds one of the keys, at this index of
    /// the store's holders, once that delivery has returned.
    Follow(usize),
    /// Through the tick, as a batch of one.
    Stage,
}

/// One delivery of a write made outside a batch. When it is dropped, also by
/// a panicking subscriber, it delivers the writes that followed it and frees
/// its keys.
pub(crate) struct Delivery<'a> {
    order: &'a Arc<WriteOrder>,
    domain: Option<&'a Arc<DomainInner>>,
    thread: usize,
    /// The thread delivers as the store's sole holder.
    solo: bool,
}

/// What a delivery finds when it returns.
enum Next {
    /// A write waiting behind this thread's deliveries to the store.
    Follower(Follower),
    /// Nothing. The keys are free when this was the thread's outermost
    /// delivery to the store, and a batch that joined it is given back.
    Done(Option<Arc<Writer>>),
}

impl Delivery<'_> {
    fn next(&self) -> Next {
        let (next, released) = self.order.with_pending(|pending| {
            let Some(index) = pending
                .held
                .iter()
                .position(|held| held.thread == self.thread)
            else {
                return (Next::Done(None), None);
            };
            let Some(held) = pending.held.get_mut(index) else {
                return (Next::Done(None), None);
            };
            // Only the outermost delivery hands on to the followers: one
            // nested in it returns while the outer one is still running.
            if held.deliveries == 1
                && let Some(follower) = held.followers.pop_front()
            {
                return (Next::Follower(follower), None);
            }
            held.deliveries = held.deliveries.saturating_sub(1);
            if held.deliveries > 0 {
                return (Next::Done(None), None);
            }
            let mut released = pending.held.swap_remove(index);
            (Next::Done(released.writer.take()), Some(released))
        });
        if let Some(held) = released {
            held.recycle();
        }
        next
    }
}

impl Drop for Delivery<'_> {
    #[inline]
    fn drop(&mut self) {
        // Nobody followed or joined a sole holder without moving it into
        // the list first.
        if self.solo && self.order.leave_solo(self.thread) {
            return;
        }
        self.finish();
    }
}

impl Delivery<'_> {
    /// Deliver the writes that followed this one, and free its keys.
    #[cold]
    fn finish(&self) {
        loop {
            match self.next() {
                Next::Follower(follower) => {
                    // The write's own thread has returned, so nobody is left
                    // to catch a panic of its subscribers.
                    if let Err(payload) =
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(follower))
                    {
                        super::domain::caught_panic(
                            self.domain,
                            "a write delivered by another thread",
                            payload.as_ref(),
                        );
                    }
                }
                // The keys are free, so no batch can join any more.
                Next::Done(Some(joined)) => {
                    return super::finish_delivering(self.domain, &joined);
                }
                Next::Done(None) => return,
            }
        }
    }
}

/// Closes the batch opened for one unbatched write, also when its
/// subscribers panic.
struct Implicit<'a>(Option<&'a Arc<DomainInner>>);

impl Drop for Implicit<'_> {
    fn drop(&mut self) {
        if let Some(payload) = super::end_batch(self.0)
            && !std::thread::panicking()
        {
            std::panic::resume_unwind(payload);
        }
    }
}

/// Hands values to one consumer in the order they were produced, whichever
/// threads produce them, with no lock held while the consumer runs.
///
/// Two threads can deliver writes to different keys of one store at the same
/// time. A cell that describes the whole store, such as its length or its
/// entries, is set through one of these, so it ends on the last value
/// produced and its subscribers see the values in order.
pub(crate) struct Sequenced<T> {
    state: parking_lot::Mutex<Sequence<T>>,
}

struct Sequence<T> {
    /// A thread is consuming, and takes what is queued behind it.
    consuming: bool,
    queued: VecDeque<T>,
}

impl<T> Default for Sequenced<T> {
    fn default() -> Self {
        Self {
            state: parking_lot::Mutex::new(Sequence {
                consuming: false,
                queued: VecDeque::new(),
            }),
        }
    }
}

impl<T> Sequenced<T> {
    /// Produce a value under the sequence's lock and have it consumed after
    /// every value produced before it. The calling thread consumes it unless
    /// another thread is consuming, which then does once it has finished its
    /// own.
    pub(crate) fn publish(&self, produce: impl FnOnce() -> Option<T>, consume: impl Fn(T)) {
        {
            let mut state = self.state.lock();
            let Some(value) = produce() else {
                return;
            };
            state.queued.push_back(value);
            if state.consuming {
                return;
            }
            state.consuming = true;
        }
        let mut consuming = Consuming {
            state: &self.state,
            armed: true,
        };
        loop {
            // Checked and released in one step, so a value queued by another
            // thread is either seen here or consumed by that thread.
            let next = {
                let mut state = self.state.lock();
                let next = state.queued.pop_front();
                if next.is_none() {
                    state.consuming = false;
                    consuming.armed = false;
                }
                drop(state);
                next
            };
            match next {
                Some(value) => consume(value),
                None => return,
            }
        }
    }
}

/// Frees the sequence when the consumer panics. What is still queued is
/// consumed by the next thread that publishes, ahead of its own value.
struct Consuming<'a, T> {
    state: &'a parking_lot::Mutex<Sequence<T>>,
    armed: bool,
}

impl<T> Drop for Consuming<'_, T> {
    fn drop(&mut self) {
        if self.armed {
            self.state.lock().consuming = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, mpsc};

    use parking_lot::Mutex;

    use super::Sequenced;

    #[test]
    fn a_value_published_while_another_is_consumed_follows_it_without_waiting() {
        let sequence = Arc::new(Sequenced::default());
        let consumed = Arc::new(Mutex::new(Vec::new()));
        let (entered, first_entered) = mpsc::channel();
        let (resume, resumed) = mpsc::channel::<()>();
        let resumed = Mutex::new(resumed);

        let first = {
            let (sequence, consumed) = (Arc::clone(&sequence), Arc::clone(&consumed));
            std::thread::spawn(move || {
                sequence.publish(
                    || Some(1),
                    |value| {
                        if value == 1 {
                            let _ = entered.send(());
                            let _ = resumed.lock().recv();
                        }
                        consumed.lock().push(value);
                    },
                );
            })
        };
        assert!(first_entered.recv().is_ok());

        // Returns although the first value is still being consumed.
        sequence.publish(|| Some(2), |value| consumed.lock().push(value));
        sequence.publish(|| None, |value| consumed.lock().push(value));
        sequence.publish(|| Some(3), |value| consumed.lock().push(value));
        assert_eq!(*consumed.lock(), Vec::<i32>::new());

        assert!(resume.send(()).is_ok());
        assert!(first.join().is_ok());
        assert_eq!(*consumed.lock(), vec![1, 2, 3]);
    }

    #[test]
    fn a_consumer_that_publishes_is_given_the_value_after_its_own() {
        let sequence = Sequenced::default();
        let consumed = Mutex::new(Vec::new());
        let consume = |value: i32| {
            consumed.lock().push(value);
            if value == 1 {
                sequence.publish(|| Some(2), |_| ());
            }
        };

        sequence.publish(|| Some(1), consume);

        assert_eq!(*consumed.lock(), vec![1, 2]);
    }

    #[test]
    fn a_panicking_consumer_leaves_the_sequence_usable() {
        let sequence = Sequenced::default();
        let consumed = Mutex::new(Vec::new());
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            sequence.publish(|| Some(1), |_: i32| std::panic::resume_unwind(Box::new(())));
        }));
        assert!(panicked.is_err());

        sequence.publish(|| Some(2), |value| consumed.lock().push(value));

        assert_eq!(*consumed.lock(), vec![2]);
    }
}
