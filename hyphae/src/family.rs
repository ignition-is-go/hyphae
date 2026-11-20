//! Families: rows built on demand per key, and `lookup` into them.
//!
//! A family maps a key to a row: a [`Cell`] (one value, like a myko report)
//! or a [`CellMap`] (a keyed collection, like a myko view). A row is built the
//! first time a reader demands its key, shared by every reader of that key,
//! and torn down by the family once no reader holds it. A row's inputs are
//! existing nodes, so a family grows the graph only at this keyed boundary.
//!
//! [`LookupExt::lookup`] reads a family at a reactive key. The reader keeps
//! two fixed inputs, the key cell and the family. When the key changes, the
//! reader takes the new key's row, built inside that fire when no reader held
//! it, and gives up the old one. A value reader then holds the new row's
//! value; a collection reader is told the new row's snapshot as one
//! `MapDiff::Initial` and after that the new row's diffs only.
//!
//! The design and its model check are in `docs/settle-epochs.md`, section 6.
//! Under ticket settling the family is a node with fixed edges from every row
//! input and from its readers' key cells: [`Family::acquire`] is the row build
//! in the family's fire at the demanding ticket, and [`Teardown`] is the drop
//! in the family's fire. On today's core teardown runs as its own write, after
//! the settle that released the last lease, so a reader that takes the key in
//! that same settle keeps the row and any state it holds.

use std::{
    collections::{BTreeSet, HashMap},
    hash::Hash,
    sync::{
        Arc, Weak,
        atomic::{AtomicU64, Ordering},
    },
};

use indexmap::{IndexMap, IndexSet};
use parking_lot::Mutex;
use uuid::Uuid;

use crate::{
    MapExt, Materialize, ProjectCellExt,
    cell::{Cell, CellImmutable, CellMutable, WeakCell},
    cell_map::{CellMap, MapDiff, WeakCellMap},
    map_query::MapQuery,
    settle::{SettleKey, Write},
    signal::Signal,
    subscription::SubscriptionGuard,
    traits::{CellValue, Gettable, Watchable},
};

/// A keyed set of rows that readers share.
pub trait Family: Clone + Send + Sync + 'static {
    type Key: CellValue + Hash + Eq;
    type Row: FamilyRow<Self::Key>;

    /// Take a lease on the row for `key`, building the row when no lease holds
    /// it. The row lives at least as long as the lease.
    fn acquire(&self, key: &Self::Key) -> Lease<Self::Row>;
}

/// A held row. Dropping the last lease on a row lets the family tear it down.
pub struct Lease<R> {
    row: R,
    release: Option<Box<dyn FnOnce() + Send + Sync>>,
}

impl<R> Lease<R> {
    pub const fn row(&self) -> &R {
        &self.row
    }
}

impl<R> Drop for Lease<R> {
    fn drop(&mut self) {
        if let Some(release) = self.release.take() {
            release();
        }
    }
}

/// The family that builds each row with a closure. See [`family`].
pub struct KeyedFamily<K, R> {
    rows: Arc<Rows<K, R>>,
}

impl<K, R> Clone for KeyedFamily<K, R> {
    fn clone(&self) -> Self {
        Self {
            rows: Arc::clone(&self.rows),
        }
    }
}

struct Rows<K, R> {
    build: Box<dyn Fn(&K) -> R + Send + Sync>,
    slots: Mutex<HashMap<K, Slot<R>>>,
}

struct Slot<R> {
    row: R,
    leases: usize,
}

/// A family whose row for `key` is `build(key)`.
///
/// `build` must be a pure function of its inputs: two calls for one key must
/// build rows that agree, because a row that nobody holds is torn down and a
/// later demand builds it again.
pub fn family<K, R>(build: impl Fn(&K) -> R + Send + Sync + 'static) -> KeyedFamily<K, R>
where
    K: CellValue + Hash + Eq,
    R: FamilyRow<K>,
{
    KeyedFamily {
        rows: Arc::new(Rows {
            build: Box::new(build),
            slots: Mutex::new(HashMap::new()),
        }),
    }
}

impl<K, R> KeyedFamily<K, R>
where
    K: CellValue + Hash + Eq,
{
    /// The number of rows built and not yet torn down.
    #[must_use]
    pub fn len(&self) -> usize {
        self.rows.slots.lock().len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether the row for `key` is built.
    #[must_use]
    pub fn contains(&self, key: &K) -> bool {
        self.rows.slots.lock().contains_key(key)
    }
}

impl<K, R> Family for KeyedFamily<K, R>
where
    K: CellValue + Hash + Eq,
    R: FamilyRow<K>,
{
    type Key = K;
    type Row = R;

    fn acquire(&self, key: &K) -> Lease<R> {
        let held = self.rows.slots.lock().get_mut(key).map(|slot| {
            slot.leases = slot.leases.saturating_add(1);
            slot.row.clone()
        });
        // Build outside the lock: a build may take leases on other families
        // or on this one.
        let row = held.unwrap_or_else(|| {
            let built = (self.rows.build)(key);
            let mut slots = self.rows.slots.lock();
            let slot = slots.entry(key.clone()).or_insert(Slot {
                row: built,
                leases: 0,
            });
            slot.leases = slot.leases.saturating_add(1);
            let row = slot.row.clone();
            drop(slots);
            row
        });
        let rows = Arc::downgrade(&self.rows);
        let key = key.clone();
        Lease {
            row,
            release: Some(Box::new(move || release(&rows, key))),
        }
    }
}

fn release<K, R>(rows: &Weak<Rows<K, R>>, key: K)
where
    K: CellValue + Hash + Eq,
    R: FamilyRow<K>,
{
    let Some(rows) = rows.upgrade() else { return };
    let unheld = {
        let mut slots = rows.slots.lock();
        slots.get_mut(&key).is_some_and(|slot| {
            slot.leases = slot.leases.saturating_sub(1);
            slot.leases == 0
        })
    };
    if unheld {
        crate::settle::submit_write(
            SettleKey::Shared,
            Box::new(Teardown {
                rows: Arc::downgrade(&rows),
                key,
            }),
        );
    }
}

/// The family's drop of a row that no lease holds. A reader that took a new
/// lease since the release keeps the row.
struct Teardown<K, R> {
    rows: Weak<Rows<K, R>>,
    key: K,
}

impl<K, R> Write for Teardown<K, R>
where
    K: CellValue + Hash + Eq,
    R: FamilyRow<K>,
{
    fn apply(self: Box<Self>) {
        let Some(rows) = self.rows.upgrade() else {
            return;
        };
        let removed = {
            let mut slots = rows.slots.lock();
            match slots.get(&self.key) {
                Some(slot) if slot.leases == 0 => slots.remove(&self.key),
                _ => None,
            }
        };
        // Dropping a row can drop cells whose drop takes other locks.
        drop(removed);
    }
}

/// How a key cell's value names a row: the key itself, or `Option<K>`, where
/// `None` names no row.
pub trait LookupKey<K>: CellValue {
    /// What a reader holds for a row output `T`.
    type Out<T: CellValue>: CellValue;

    /// The row this value names.
    ///
    /// # Errors
    ///
    /// When it names no row, what a reader then holds.
    fn name<T: CellValue>(&self) -> Result<&K, Self::Out<T>>;

    /// What a reader holds for a row value.
    fn wrap<T: CellValue>(row: Arc<T>) -> Arc<Self::Out<T>>;
}

impl<K: CellValue> LookupKey<K> for K {
    type Out<T: CellValue> = T;

    fn name<T: CellValue>(&self) -> Result<&K, T> {
        Ok(self)
    }

    fn wrap<T: CellValue>(row: Arc<T>) -> Arc<T> {
        row
    }
}

impl<K: CellValue> LookupKey<K> for Option<K> {
    type Out<T: CellValue> = Option<T>;

    fn name<T: CellValue>(&self) -> Result<&K, Option<T>> {
        self.as_ref().ok_or(None)
    }

    fn wrap<T: CellValue>(row: Arc<T>) -> Arc<Option<T>> {
        Arc::new(Some(Arc::unwrap_or_clone(row)))
    }
}

/// A row shape: what a reader of it is, and how it follows a key.
pub trait FamilyRow<K>: Clone + Send + Sync + 'static {
    /// The reader for a key cell of form `Q`.
    type Reader<Q: LookupKey<K>>;

    #[doc(hidden)]
    fn read<F, Q, M>(key: Cell<Q, M>, family: F) -> Self::Reader<Q>
    where
        F: Family<Key = K, Row = Self>,
        Q: LookupKey<K>,
        M: Send + Sync + 'static;
}

/// Read a family at a reactive key.
pub trait LookupExt<Q> {
    /// The row for the key at each moment: its value for a [`Cell`] row, or
    /// its entries for a [`CellMap`] row. A `None` key reads as `None` for a
    /// value row and as no entries for a collection row.
    fn lookup<F>(self, family: F) -> <F::Row as FamilyRow<F::Key>>::Reader<Q>
    where
        F: Family,
        Q: LookupKey<F::Key>;
}

impl<Q: CellValue, M: Send + Sync + 'static> LookupExt<Q> for Cell<Q, M> {
    fn lookup<F>(self, family: F) -> <F::Row as FamilyRow<F::Key>>::Reader<Q>
    where
        F: Family,
        Q: LookupKey<F::Key>,
    {
        <F::Row as FamilyRow<F::Key>>::read(self, family)
    }
}

/// The state every reader shares: which key it follows, and which
/// generation of attachment may still deliver.
///
/// A switch bumps the generation under `switch_lock`, and a row's delivery
/// checks it under the same lock, so the old row cannot deliver behind the new
/// one. `current` is held across a whole switch, so two switches never
/// interleave.
struct Follow<K> {
    generation: AtomicU64,
    switch_lock: Mutex<()>,
    current: Mutex<Option<K>>,
    guard_key: Uuid,
}

impl<K: Clone + PartialEq> Follow<K> {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            generation: AtomicU64::new(0),
            switch_lock: Mutex::new(()),
            current: Mutex::new(None),
            guard_key: Uuid::new_v4(),
        })
    }

    /// Start following `next`. The generation of the new attachment, or
    /// `None` when the reader already follows that key. `current` is the
    /// caller's hold on [`Follow::current`].
    fn advance(&self, current: &mut Option<K>, next: Option<&K>) -> Option<u64> {
        if next.is_some() && current.as_ref() == next {
            return None;
        }
        current.clone_from(&next.cloned());
        let _switch = self.switch_lock.lock();
        Some(
            self.generation
                .fetch_add(1, Ordering::SeqCst)
                .wrapping_add(1),
        )
    }

    fn live(&self, generation: u64) -> bool {
        self.generation.load(Ordering::SeqCst) == generation
    }
}

impl<K, T, RM> FamilyRow<K> for Cell<T, RM>
where
    K: CellValue,
    T: CellValue,
    RM: Send + Sync + 'static,
{
    type Reader<Q: LookupKey<K>> = Cell<Q::Out<T>, CellImmutable>;

    fn read<F, Q, M>(key: Cell<Q, M>, family: F) -> Self::Reader<Q>
    where
        F: Family<Key = K, Row = Self>,
        Q: LookupKey<K>,
        M: Send + Sync + 'static,
    {
        // Held until the key's replay has taken its own lease, so the row is
        // built once.
        let first = key.get();
        let first = Q::name::<T>(&first).map(|k| family.acquire(k));
        let initial = match &first {
            Ok(lease) => Arc::unwrap_or_clone(Q::wrap(Arc::new(lease.row().get()))),
            Err(none) => none.clone(),
        };
        let out = Cell::<Q::Out<T>, CellMutable>::new(initial).combine_latest();
        let follow = Follow::<K>::new();
        let weak = out.downgrade();
        let key_guard = key.subscribe(move |signal| match signal {
            Signal::Value(q) => switch_value(&weak, &follow, &family, q),
            Signal::Error(error) => {
                if let Some(out) = weak.upgrade() {
                    out.notify(Signal::Error(Arc::clone(error)));
                }
            }
            Signal::Complete => {}
        });
        out.own(key_guard);
        drop(first);
        out.lock()
    }
}

fn switch_value<F, Q, T, RM>(
    weak: &WeakCell<Q::Out<T>, CellMutable>,
    follow: &Arc<Follow<F::Key>>,
    family: &F,
    q: &Arc<Q>,
) where
    F: Family<Row = Cell<T, RM>>,
    Q: LookupKey<F::Key>,
    T: CellValue,
    RM: Send + Sync + 'static,
{
    let Some(out) = weak.upgrade() else { return };
    let mut current = follow.current.lock();
    let next = Q::name::<T>(q);
    let Some(generation) = follow.advance(&mut current, next.as_ref().ok().copied()) else {
        return;
    };
    let next = match next {
        Ok(next) => next,
        Err(none) => {
            out.release_owned(follow.guard_key);
            push_value::<F::Key, Q, T>(&out, follow, generation, Arc::new(none));
            drop(current);
            return;
        }
    };
    let lease = family.acquire(next);
    let deliver = {
        let weak = weak.clone();
        let follow = Arc::clone(follow);
        move |signal: &Signal<T>| {
            let Some(out) = weak.upgrade() else { return };
            match signal {
                Signal::Value(value) => {
                    push_value::<F::Key, Q, T>(
                        &out,
                        &follow,
                        generation,
                        Q::wrap(Arc::clone(value)),
                    );
                }
                Signal::Error(error) => {
                    let error = Arc::clone(error);
                    let follow = Arc::clone(&follow);
                    out.write_with(move |push| {
                        let _switch = follow.switch_lock.lock();
                        if follow.live(generation) {
                            push(Signal::Error(error));
                        }
                    });
                }
                Signal::Complete => {}
            }
        }
    };
    // The row replays its value as it subscribes, so the reader holds the
    // row's value as of this fire.
    let guard = lease.row().subscribe(deliver);
    let mut lease = Some(lease);
    out.own_keyed(
        follow.guard_key,
        guard.with_cleanup(move || drop(lease.take())),
    );
    drop(current);
}

fn push_value<K, Q, T>(
    out: &Cell<Q::Out<T>, CellMutable>,
    follow: &Arc<Follow<K>>,
    generation: u64,
    value: Arc<Q::Out<T>>,
) where
    K: CellValue,
    Q: LookupKey<K>,
    T: CellValue,
{
    let follow = Arc::clone(follow);
    out.write_with(move |push| {
        let _switch = follow.switch_lock.lock();
        if follow.live(generation) {
            push(Signal::Value(value));
        }
    });
}

impl<K, RK, V, RM> FamilyRow<K> for CellMap<RK, V, RM>
where
    K: CellValue,
    RK: CellValue + Hash + Eq,
    V: CellValue,
    RM: Clone + Send + Sync + 'static,
{
    type Reader<Q: LookupKey<K>> = CellMap<RK, V, CellImmutable>;

    fn read<F, Q, M>(key: Cell<Q, M>, family: F) -> Self::Reader<Q>
    where
        F: Family<Key = K, Row = Self>,
        Q: LookupKey<K>,
        M: Send + Sync + 'static,
    {
        let out = CellMap::<RK, V, CellMutable>::new();
        let follow = Follow::<K>::new();
        let weak = out.downgrade();
        let key_guard = key.subscribe(move |signal| {
            if let Signal::Value(q) = signal {
                switch_map_row(&weak, &follow, &family, Q::name::<()>(q).ok());
            }
        });
        out.own(key_guard);
        out.lock()
    }
}

fn switch_map_row<F, RK, V, RM>(
    weak: &WeakCellMap<RK, V>,
    follow: &Arc<Follow<F::Key>>,
    family: &F,
    next: Option<&F::Key>,
) where
    F: Family<Row = CellMap<RK, V, RM>>,
    RK: CellValue + Hash + Eq,
    V: CellValue,
    RM: Clone + Send + Sync + 'static,
{
    let Some(out) = weak.upgrade() else { return };
    let mut current = follow.current.lock();
    let Some(generation) = follow.advance(&mut current, next) else {
        return;
    };
    let Some(next) = next else {
        out.inner.diffs_cell.release_owned(follow.guard_key);
        out.apply_diff_owned(MapDiff::Initial {
            entries: Vec::new(),
        });
        drop(current);
        return;
    };
    let lease = family.acquire(next);
    let deliver = {
        let weak = out.downgrade();
        let follow = Arc::clone(follow);
        move |diff: &MapDiff<RK, V>| {
            let Some(out) = weak.upgrade() else { return };
            let _switch = follow.switch_lock.lock();
            if follow.live(generation) {
                out.apply_diff_owned(diff.clone());
            }
        }
    };
    // `subscribe_diffs` tells the row's snapshot first, as one `Initial`, and
    // then every diff after it: a change already in the snapshot is not told
    // again.
    let guard = lease.row().subscribe_diffs(deliver);
    let mut lease = Some(lease);
    out.inner.diffs_cell.own_keyed(
        follow.guard_key,
        guard.with_cleanup(move || drop(lease.take())),
    );
    drop(current);
}

/// A key set [`LookupSetExt::lookup_set`] can join over. Its iteration order
/// is the order of the joined map, so it must be deterministic.
pub trait KeySet<K: 'static>: CellValue {
    fn keys(&self) -> impl Iterator<Item = &K>;
}

impl<K: CellValue + Ord> KeySet<K> for BTreeSet<K> {
    fn keys(&self) -> impl Iterator<Item = &K> {
        self.iter()
    }
}

impl<K: CellValue + Hash + Eq> KeySet<K> for IndexSet<K> {
    fn keys(&self) -> impl Iterator<Item = &K> {
        self.iter()
    }
}

/// Join a family over a reactive key set.
pub trait LookupSetExt<K> {
    /// The value of every row whose key is in the set, by key, in the set's
    /// iteration order. A row enters and leaves the map with its key, and the
    /// map updates when a row in it changes.
    fn lookup_set<F, T, RM>(self, family: F) -> Cell<IndexMap<K, T>, CellImmutable>
    where
        F: Family<Key = K, Row = Cell<T, RM>>,
        T: CellValue,
        RM: Send + Sync + 'static;
}

struct Member<T> {
    attach: u64,
    guard_key: Uuid,
    value: Option<Arc<T>>,
}

struct Members<K, T> {
    rows: Mutex<IndexMap<K, Member<T>>>,
    attaches: AtomicU64,
}

impl<K: Hash + Eq + Clone, T: Clone> Members<K, T> {
    fn snapshot(rows: &IndexMap<K, Member<T>>) -> IndexMap<K, T> {
        rows.iter()
            .filter_map(|(key, member)| {
                member
                    .value
                    .as_ref()
                    .map(|value| (key.clone(), T::clone(value)))
            })
            .collect()
    }
}

impl<K, S, M> LookupSetExt<K> for Cell<S, M>
where
    K: CellValue + Hash + Eq,
    S: KeySet<K>,
    M: Send + Sync + 'static,
{
    fn lookup_set<F, T, RM>(self, family: F) -> Cell<IndexMap<K, T>, CellImmutable>
    where
        F: Family<Key = K, Row = Cell<T, RM>>,
        T: CellValue,
        RM: Send + Sync + 'static,
    {
        let out = Cell::<IndexMap<K, T>, CellMutable>::new(IndexMap::new()).combine_latest();
        let members = Arc::new(Members {
            rows: Mutex::new(IndexMap::new()),
            attaches: AtomicU64::new(0),
        });
        let weak = out.downgrade();
        let key_guard = self.subscribe(move |signal| {
            if let Signal::Value(keys) = signal {
                switch_set(&weak, &members, &family, &**keys);
            }
        });
        out.own(key_guard);
        out.lock()
    }
}

fn switch_set<F, K, S, T, RM>(
    weak: &WeakCell<IndexMap<K, T>, CellMutable>,
    members: &Arc<Members<K, T>>,
    family: &F,
    keys: &S,
) where
    F: Family<Key = K, Row = Cell<T, RM>>,
    K: CellValue + Hash + Eq,
    S: KeySet<K>,
    T: CellValue,
    RM: Send + Sync + 'static,
{
    let Some(out) = weak.upgrade() else { return };
    let (left, entered) = {
        let mut rows = members.rows.lock();
        // Rebuild in the set's order, keeping members that stay.
        let mut next = IndexMap::with_capacity(rows.len());
        let mut entered = Vec::new();
        for key in keys.keys() {
            if next.contains_key(key) {
                continue;
            }
            if let Some(member) = rows.swap_remove(key) {
                next.insert(key.clone(), member);
                continue;
            }
            let attach = members.attaches.fetch_add(1, Ordering::Relaxed);
            let guard_key = Uuid::new_v4();
            next.insert(
                key.clone(),
                Member {
                    attach,
                    guard_key,
                    value: None,
                },
            );
            entered.push((key.clone(), attach, guard_key));
        }
        let left: Vec<Uuid> = std::mem::replace(&mut *rows, next)
            .into_values()
            .map(|member| member.guard_key)
            .collect();
        drop(rows);
        (left, entered)
    };
    for guard_key in left {
        out.release_owned(guard_key);
    }
    for (key, attach, guard_key) in entered {
        let lease = family.acquire(&key);
        let deliver = {
            let weak = weak.clone();
            let members = Arc::clone(members);
            move |signal: &Signal<T>| {
                let (Signal::Value(value), Some(out)) = (signal, weak.upgrade()) else {
                    return;
                };
                let value = Arc::clone(value);
                let members = Arc::clone(&members);
                let key = key.clone();
                out.write_with(move |push| {
                    let mut rows = members.rows.lock();
                    let Some(member) = rows.get_mut(&key).filter(|m| m.attach == attach) else {
                        return;
                    };
                    member.value = Some(value);
                    // Pushed under the lock, so a later snapshot is never
                    // pushed ahead of this one.
                    push(Signal::value(Members::snapshot(&rows)));
                    drop(rows);
                });
            }
        };
        let guard = lease.row().subscribe(deliver);
        let mut lease = Some(lease);
        out.own_keyed(guard_key, guard.with_cleanup(move || drop(lease.take())));
    }
    let members = Arc::clone(members);
    out.write_with(move |push| {
        let rows = members.rows.lock();
        push(Signal::value(Members::snapshot(&rows)));
    });
}

/// Join a family per entry of a collection.
pub trait LookupEachExt<K, V>: MapQuery<Key = K, Value = V>
where
    K: CellValue + Hash + Eq,
    V: CellValue,
{
    /// For each entry `(k, v)`, the value of the row at `key(k, v)`, under
    /// `k`. An entry holds a lease on its row while it is present, so rows
    /// are built as entries arrive, shared by entries with the same family
    /// key, and released as entries leave or rekey. An entry and its row
    /// changing in one settle are told once, with the row's new value.
    #[track_caller]
    fn lookup_each<F, T, RM, KF>(self, family: F, key: KF) -> impl MapQuery<Key = K, Value = T>
    where
        Self: Sized,
        F: Family<Row = Cell<T, RM>>,
        T: CellValue,
        RM: Send + Sync + 'static,
        KF: Fn(&K, &V) -> F::Key + Send + Sync + 'static,
    {
        self.project_cell(move |k: &K, v: &V| {
            let lease = family.acquire(&key(k, v));
            let k = k.clone();
            let row = lease
                .row()
                .clone()
                .map(move |t: &T| Some((k.clone(), t.clone())))
                .materialize();
            let mut lease = Some(lease);
            row.own(SubscriptionGuard::from_callback(move || drop(lease.take())));
            row
        })
    }
}

impl<K, V, Q> LookupEachExt<K, V> for Q
where
    K: CellValue + Hash + Eq,
    V: CellValue,
    Q: MapQuery<Key = K, Value = V>,
{
}
