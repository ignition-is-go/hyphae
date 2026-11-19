//! Telling a collection's subscribers what changed, in the order the data
//! changed.
//!
//! A mutation that joins the settle in progress writes its diff at once. Any
//! other mutation is delivered by a later settle, and the event set that
//! carries it can reach the input queue after another thread's later
//! mutation: a `batch`, `submit`, or `enqueue` queues when it closes. A diff
//! built at mutation time would then arrive out of order.
//!
//! So such a mutation queues no diff. It records, for each key it changes,
//! the value subscribers were last told, and queues a request to publish
//! those keys. When a settle applies the request, the collection reads its
//! stored data and tells subscribers the step from what they were told to
//! what is stored. Every diff therefore starts from what subscribers hold,
//! and once every queued request has been applied they hold the stored data,
//! in whatever order the requests arrived.
//!
//! `docs/settle-core.md` section 5 states what a subscriber can rely on.
//!
//! The state here is borrowed only around its own map operations. Code of
//! the key and value types runs with it released, except a key's `Hash` and
//! `Eq`. Those may read the collection, and must not write it.

use std::{cell::RefCell, collections::hash_map::Entry, hash::Hash};

use parking_lot::ReentrantMutex;
use rustc_hash::FxHashMap;

use crate::{cell::Cell, settle::Position};

/// One key with changes that subscribers have not been told.
struct Told<V> {
    /// The value subscribers hold; `None` when they hold no entry.
    value: Option<V>,
    /// The queued publish requests that name this key.
    queued: usize,
    /// The last mutation that counted itself in `queued`.
    pass: u64,
}

/// What subscribers held for a key before a publish told them the stored
/// value; `None` inside when they held no entry.
pub struct Held<V>(pub Option<V>);

/// The keys of one collection whose stored value subscribers have not been
/// told, and the length they were last told.
///
/// A key is here from the mutation that changes it until the last publish
/// request that names it has been applied. A key that is absent has no change
/// waiting: subscribers hold its stored value.
pub struct InFlight<K, V> {
    keys: FxHashMap<K, Told<V>>,
    passes: u64,
    /// The number of entries stored. Counted here because the length of a
    /// sharded map is a pass over every shard.
    stored: usize,
    /// The length subscribers were last told.
    told_len: usize,
}

/// A collection's write lock and the state it guards.
///
/// Held from a change of the stored data until the diff or the publish
/// request for it is queued, and while a publish request is applied. No
/// subscriber runs under it. It is reentrant because one mutation can be
/// built from another.
pub type WriteOrder<K, V> = ReentrantMutex<RefCell<InFlight<K, V>>>;

impl<K: Hash + Eq + Clone, V: Clone + PartialEq> InFlight<K, V> {
    /// The state of a collection that holds `len` entries and has told its
    /// subscribers all of them.
    pub fn new(len: usize) -> WriteOrder<K, V> {
        ReentrantMutex::new(RefCell::new(Self {
            keys: FxHashMap::default(),
            passes: 0,
            stored: len,
            told_len: len,
        }))
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    pub fn holds(&self, key: &K) -> bool {
        self.keys.contains_key(key)
    }

    /// What subscribers hold for `key`, when a change to it waits to be
    /// published. `None` when none does, and then they hold the stored value.
    ///
    /// Also `None` when the state is in use further up this thread's stack:
    /// a key whose `Hash` or `Eq` reads the collection gets the stored value.
    pub fn told(flight: &RefCell<Self>, key: &K) -> Option<Held<V>> {
        let told = flight.try_borrow_mut().ok()?.keys.remove(key)?;
        let value = told.value.clone();
        let key = key.clone();
        flight.borrow_mut().restore(key, told);
        Some(Held(value))
    }

    /// Put back an entry that was taken out, or add one that was built,
    /// while code of the key or value type ran. If that code changed the key
    /// meanwhile, the entry it made keeps its count, and subscribers still
    /// hold what `told` says.
    fn restore(&mut self, key: K, told: Told<V>) {
        match self.keys.entry(key) {
            Entry::Occupied(mut newer) => {
                let newer = newer.get_mut();
                newer.queued = newer.queued.saturating_add(told.queued);
                newer.value = told.value;
            }
            Entry::Vacant(vacant) => {
                vacant.insert(told);
            }
        }
    }

    /// Start one mutation. The result names it in [`touch`](Self::touch).
    pub fn begin(flight: &RefCell<Self>) -> u64 {
        let mut flight = flight.borrow_mut();
        flight.passes = flight.passes.wrapping_add(1);
        flight.passes
    }

    /// Record that mutation `pass` changed `key` from `old`. True the first
    /// time the mutation touches the key: its publish request must name the
    /// key once.
    pub fn touch(flight: &RefCell<Self>, pass: u64, key: &K, old: Option<&V>) -> bool {
        if let Some(told) = flight.borrow_mut().keys.get_mut(key) {
            if told.pass == pass {
                return false;
            }
            told.pass = pass;
            told.queued = told.queued.saturating_add(1);
            return true;
        }
        let told = Told {
            value: old.cloned(),
            queued: 1,
            pass,
        };
        let key = key.clone();
        flight.borrow_mut().restore(key, told);
        true
    }

    /// Apply one publish request for `key`, whose stored value is `live`.
    /// Returns what subscribers held when that differs from `live`; they are
    /// now recorded as holding `live`.
    pub fn land(flight: &RefCell<Self>, key: &K, live: Option<&V>) -> Option<Held<V>> {
        let mut told = flight.borrow_mut().keys.remove(key)?;
        let previous = (told.value.as_ref() != live)
            .then(|| Held(std::mem::replace(&mut told.value, live.cloned())));
        told.queued = told.queued.saturating_sub(1);
        if told.queued > 0 {
            let key = key.clone();
            flight.borrow_mut().restore(key, told);
        }
        previous
    }

    /// Record that subscribers now hold the stored data for every key, after
    /// they were sent all of it. `live` reads a key's stored value, and
    /// `changed` runs for each key whose recorded value this changes.
    pub fn reset(
        flight: &RefCell<Self>,
        live: impl Fn(&K) -> Option<V>,
        mut changed: impl FnMut(&K, &Option<V>),
    ) {
        let mut keys = std::mem::take(&mut flight.borrow_mut().keys);
        for (key, told) in &mut keys {
            let live = live(key);
            if told.value != live {
                changed(key, &live);
                told.value = live;
            }
        }
        let mut flight = flight.borrow_mut();
        let added = std::mem::replace(&mut flight.keys, keys);
        for (key, told) in added {
            flight.restore(key, told);
        }
    }

    /// Count one stored entry that a mutation added or removed.
    pub fn resize(flight: &RefCell<Self>, was_stored: bool, is_stored: bool) {
        let mut flight = flight.borrow_mut();
        match (was_stored, is_stored) {
            (false, true) => flight.stored = flight.stored.saturating_add(1),
            (true, false) => flight.stored = flight.stored.saturating_sub(1),
            _ => {}
        }
    }

    /// The stored length, when subscribers were last told another. They are
    /// now recorded as holding it.
    pub fn tell_len(flight: &RefCell<Self>) -> Option<usize> {
        let mut flight = flight.borrow_mut();
        let len = flight.stored;
        (std::mem::replace(&mut flight.told_len, len) != len).then_some(len)
    }
}

/// Run one collection mutation under the collection's write lock.
///
/// `write` changes the stored data and writes the diff, or queues a publish
/// request. The writes it makes outside a settle and outside a batch are
/// queued as one event set before the lock is released, and settled after.
/// No subscriber runs under the lock.
pub fn write_in_order<K, V, R>(
    order: &WriteOrder<K, V>,
    write: impl FnOnce(&RefCell<InFlight<K, V>>) -> R,
) -> R {
    let held = order.lock();
    let (result, queued) = crate::settle::atomic_queued(|| write(&held));
    drop(held);
    queued.finish();
    result
}

/// Whether a write to `cell` made now joins the settle in progress on this
/// thread. Otherwise a later settle delivers it.
pub fn joins_settle<T: crate::traits::CellValue, M>(cell: &Cell<T, M>) -> bool {
    // A `Queue` position counts this write as queued until it is dropped,
    // which is here: the caller queues a publish request, not this write.
    matches!(
        crate::settle::position(cell.inner.as_ref(), &cell.inner.backlog),
        Position::Joins
    )
}
