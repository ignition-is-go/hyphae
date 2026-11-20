//! Families: rows built on demand per key, and `lookup` into them.
//!
//! A family maps a key to a row: a [`Cell`] (one value, like a myko report)
//! or a [`CellMap`] (a keyed collection, like a myko view). A row is built the
//! first time a reader demands its key, shared by every reader of that key,
//! and torn down by the family once no reader holds it. A row's inputs are
//! existing nodes, so a family grows the graph only at this keyed boundary.
//!
//! [`LookupExt::lookup`] reads a family at a reactive key. When the key
//! changes, the reader takes the new key's row and gives up the old one. A
//! value reader then holds the new row's value; a collection reader is told
//! the new row's snapshot as one `MapDiff::Initial` and after that the new
//! row's diffs only.
//!
//! The rules are `docs/settle-epochs.md` section 6:
//!
//! - **Anchor.** The family is a node. [`KeyedFamily::anchor`] gives it a
//!   static edge from an input its rows read, and every ticket that input
//!   fires is delivered to it. Each reader's key cell feeds it too, and each
//!   reader hears it.
//! - **Build in the ticket.** A key change reaches the family's fire at
//!   ticket `N`, and the row is built there. It reads every input at `N`,
//!   processes `N`, promises `N + 1`, and is sent each ticket above `N` that
//!   an input already fired.
//! - **Teardown.** The row is dropped in the family's own fire: at `N` when
//!   the last lease is given up in that fire, otherwise as a later ticket.

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
    cell::{Cell, CellImmutable, CellMutable, WeakCell},
    cell_map::{CellMap, MapDiff, WeakCellMap},
    map_query::MapQuery,
    settle::Node,
    signal::Signal,
    subscription::SubscriptionGuard,
    traits::{CellValue, Gettable, Mutable, Watchable},
};

/// A keyed set of rows that readers share.
pub trait Family: Clone + Send + Sync + 'static {
    type Key: CellValue + Hash + Eq;
    type Row: FamilyRow<Self::Key>;

    /// Take a lease on the row for `key`, building the row when no lease holds
    /// it. The row lives at least as long as the lease.
    fn acquire(&self, key: &Self::Key) -> Lease<Self::Row>;

    /// The family's node, whose fire builds and drops rows.
    fn node(&self) -> FamilyNode;
}

/// A family's node. Readers' demand flows into it, and it builds and drops
/// rows in its fire.
#[derive(Clone)]
pub struct FamilyNode(Cell<(), CellMutable>);

impl FamilyNode {
    fn new() -> Self {
        Self(Cell::new(()))
    }

    fn dyn_node(&self) -> Arc<dyn Node> {
        self.0.inner.clone()
    }

    /// Run `f`, which builds rows, as a row build in this node's fire.
    fn build<R>(&self, f: impl FnOnce() -> R) -> R {
        crate::settle::build_rows(&self.0.inner.core, f)
    }

    /// Make `guard`'s callback run in this node's fire, until the returned
    /// guard drops.
    fn hold(&self, guard: SubscriptionGuard) -> SubscriptionGuard {
        let key = Uuid::new_v4();
        self.0.own_keyed(key, guard);
        let node = self.0.downgrade();
        SubscriptionGuard::from_callback(move || {
            if let Some(node) = node.upgrade() {
                node.release_owned(key);
            }
        })
    }

    /// Give `reader` a gated edge from this node, gated by `gate`, a node
    /// that delivers each of the reader's demand changes. The reader waits
    /// on the family only at a ticket that changed its own demand, the only
    /// ticket at which the family writes it. Returns the subscription, for
    /// the reader to own, through which `gate` delivers those tickets.
    fn gate<T: CellValue>(
        &self,
        reader: &Arc<crate::settle::NodeCore>,
        gate: &Cell<T, impl Send + Sync + 'static>,
    ) -> SubscriptionGuard {
        let (src, gate_core) = (Arc::clone(&self.0.inner.core), Arc::clone(&gate.inner.core));
        reader.add_gated_input(&src, &gate_core);
        let reader = Arc::downgrade(reader);
        // The gated edge lives as long as the gate's subscription.
        gate.subscribe(|_| {}).with_cleanup(move || {
            if let Some(reader) = reader.upgrade() {
                reader.remove_gated_input(&src, &gate_core);
            }
        })
    }
}

/// A node a family can anchor to: an input its rows read.
pub trait FamilyInput {
    /// A subscription that delivers every ticket this input fires.
    fn anchor_guard(&self) -> SubscriptionGuard;
}

impl<T: CellValue, M: Send + Sync + 'static> FamilyInput for Cell<T, M> {
    fn anchor_guard(&self) -> SubscriptionGuard {
        self.subscribe(|_| {})
    }
}

impl<K, V, M> FamilyInput for CellMap<K, V, M>
where
    K: CellValue + Hash + Eq,
    V: CellValue,
{
    fn anchor_guard(&self) -> SubscriptionGuard {
        self.inner.diffs_cell.subscribe(|_| {})
    }
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
    node: FamilyNode,
    /// Keys whose last lease was given up in the family's fire now running.
    /// The fire's fanout drops the ones still unheld, so a key released and
    /// demanded in one ticket keeps its row.
    dropping: Mutex<Vec<K>>,
    /// The family node's own subscription that runs that drop.
    sweep: Mutex<Option<SubscriptionGuard>>,
}

struct Slot<R> {
    row: R,
    leases: usize,
    /// The key under which the family node owns the row's watermark edge.
    edge: Uuid,
}

/// A family whose row for `key` is `build(key)`.
///
/// `build` must be a pure function of its inputs: two calls for one key must
/// build rows that agree, because a row that nobody holds is torn down and a
/// later demand builds it again. Declare every input `build` reads with
/// [`KeyedFamily::anchor`]; debug builds panic on a row that reads a node no
/// anchor covers.
pub fn family<K, R>(build: impl Fn(&K) -> R + Send + Sync + 'static) -> KeyedFamily<K, R>
where
    K: CellValue + Hash + Eq,
    R: FamilyRow<K>,
{
    let rows = Arc::new(Rows {
        build: Box::new(build),
        slots: Mutex::new(HashMap::new()),
        node: FamilyNode::new(),
        dropping: Mutex::new(Vec::new()),
        sweep: Mutex::new(None),
    });
    let weak = Arc::downgrade(&rows);
    let sweep = rows.node.0.subscribe(move |_| teardown(&weak));
    *rows.sweep.lock() = Some(sweep);
    KeyedFamily { rows }
}

impl<K, R> KeyedFamily<K, R>
where
    K: CellValue + Hash + Eq,
{
    /// Anchor the family to `input`, a node its rows read: the family node
    /// gets a static edge from it and hears every ticket it fires. A row
    /// built at ticket `N` is then sound against tickets above `N` that the
    /// input fires during the build. An input is covered when it, or every
    /// node its value derives from, is anchored.
    pub fn anchor(&self, input: &impl FamilyInput) -> &Self {
        self.rows.node.0.own(input.anchor_guard());
        self
    }

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
            let built = self.rows.node.build(|| (self.rows.build)(key));
            let mut slots = self.rows.slots.lock();
            let slot = slots.entry(key.clone()).or_insert_with(|| {
                let edge = Uuid::new_v4();
                self.rows.node.0.own_keyed(edge, built.watermark_edge());
                Slot {
                    row: built,
                    leases: 0,
                    edge,
                }
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

    fn node(&self) -> FamilyNode {
        self.rows.node.clone()
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
        let weak = Arc::downgrade(&rows);
        crate::settle::run_in_fire(
            &rows.node.dyn_node(),
            Box::new(move || {
                if let Some(rows) = weak.upgrade() {
                    rows.dropping.lock().push(key);
                    rows.node.0.set(());
                }
            }),
        );
    }
}

/// The family's drop, at the fanout of its fire, of each released row that
/// no lease holds. A reader that took a new lease in the fire keeps the row.
fn teardown<K, R>(rows: &Weak<Rows<K, R>>)
where
    K: CellValue + Hash + Eq,
{
    let Some(rows) = rows.upgrade() else { return };
    let dropping = std::mem::take(&mut *rows.dropping.lock());
    let removed: Vec<Slot<R>> = {
        let mut slots = rows.slots.lock();
        dropping
            .iter()
            .filter_map(|key| match slots.get(key) {
                Some(slot) if slot.leases == 0 => slots.remove(key),
                _ => None,
            })
            .collect()
    };
    for slot in &removed {
        rows.node.0.release_owned(slot.edge);
    }
    // Dropping a row can drop cells whose drop takes other locks.
    drop(removed);
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

    /// An edge from the row's output into its owner that carries no data:
    /// the family's fire waits for the row, so the row has delivered every
    /// ticket up to the one the family drops it at.
    #[doc(hidden)]
    fn watermark_edge(&self) -> SubscriptionGuard;
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
/// The family's fire issues a generation at the switch ticket `T`, and the
/// reader's own fire at `T` makes it live. A delivery is checked in the
/// reader's fire too, so the old row's deliveries below `T` still apply and
/// its delivery at `T` does not: the family's fire can run ahead of the
/// reader, which hears it only at tickets that change its key.
struct Follow<K> {
    generation: AtomicU64,
    issued: AtomicU64,
    current: Mutex<Option<K>>,
    guard_key: Uuid,
}

impl<K: Clone + PartialEq> Follow<K> {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            generation: AtomicU64::new(0),
            issued: AtomicU64::new(0),
            current: Mutex::new(None),
            guard_key: Uuid::new_v4(),
        })
    }

    /// Start following `next`. The generation of the new attachment, or
    /// `None` when the reader already follows that key. `current` is the
    /// caller's hold on [`Follow::current`].
    fn advance(
        self: &Arc<Self>,
        current: &mut Option<K>,
        next: Option<&K>,
        reader: &Arc<dyn Node>,
    ) -> Option<u64>
    where
        K: Send + Sync + 'static,
    {
        if next.is_some() && current.as_ref() == next {
            return None;
        }
        current.clone_from(&next.cloned());
        let generation = self.issued.fetch_add(1, Ordering::SeqCst).wrapping_add(1);
        let follow = Arc::clone(self);
        crate::settle::run_in_fire(
            reader,
            Box::new(move || follow.generation.store(generation, Ordering::SeqCst)),
        );
        Some(generation)
    }

}

impl<K> Follow<K> {
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

    fn watermark_edge(&self) -> SubscriptionGuard {
        SubscriptionGuard::dependency(self)
    }

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
        let node = family.node();
        out.own(node.gate(&out.inner.core, &key));
        let follow = Follow::<K>::new();
        let weak = out.downgrade();
        let builder = node.clone();
        let key_guard = key.subscribe(move |signal| match signal {
            Signal::Value(q) => builder.build(|| switch_value(&weak, &follow, &family, q)),
            Signal::Error(error) => {
                if let Some(out) = weak.upgrade() {
                    out.notify(Signal::Error(Arc::clone(error)));
                }
            }
            Signal::Complete => {}
        });
        out.own(node.hold(key_guard));
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
    let reader: Arc<dyn Node> = out.inner.clone();
    let mut current = follow.current.lock();
    let next = Q::name::<T>(q);
    let Some(generation) = follow.advance(&mut current, next.as_ref().ok().copied(), &reader)
    else {
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
                    let target = out.downgrade();
                    let reader: Arc<dyn Node> = out.inner.clone();
                    crate::settle::run_in_fire(
                        &reader,
                        Box::new(move || {
                            if follow.live(generation)
                                && let Some(out) = target.upgrade()
                            {
                                out.notify(Signal::Error(error));
                            }
                        }),
                    );
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
    let target = out.downgrade();
    let reader: Arc<dyn Node> = out.inner.clone();
    crate::settle::run_in_fire(
        &reader,
        Box::new(move || {
            if follow.live(generation)
                && let Some(out) = target.upgrade()
            {
                out.inner.write(Signal::Value(value), true);
            }
        }),
    );
}

impl<K, RK, V, RM> FamilyRow<K> for CellMap<RK, V, RM>
where
    K: CellValue,
    RK: CellValue + Hash + Eq,
    V: CellValue,
    RM: Clone + Send + Sync + 'static,
{
    type Reader<Q: LookupKey<K>> = CellMap<RK, V, CellImmutable>;

    fn watermark_edge(&self) -> SubscriptionGuard {
        SubscriptionGuard::dependency(&self.inner.diffs_cell)
    }

    fn read<F, Q, M>(key: Cell<Q, M>, family: F) -> Self::Reader<Q>
    where
        F: Family<Key = K, Row = Self>,
        Q: LookupKey<K>,
        M: Send + Sync + 'static,
    {
        let out = CellMap::<RK, V, CellMutable>::new();
        let node = family.node();
        out.inner
            .diffs_cell
            .own(node.gate(&out.inner.diffs_cell.inner.core, &key));
        let follow = Follow::<K>::new();
        let weak = out.downgrade();
        let builder = node.clone();
        let key_guard = key.subscribe(move |signal| {
            if let Signal::Value(q) = signal {
                builder.build(|| switch_row(&weak, &follow, &family, Q::name::<()>(q).ok()));
            }
        });
        out.own_guard(node.hold(key_guard));
        out.lock()
    }
}

fn switch_row<F, RK, V, RM>(
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
    let reader: Arc<dyn Node> = out.inner.diffs_cell.inner.clone();
    let mut current = follow.current.lock();
    let Some(generation) = follow.advance(&mut current, next, &reader) else {
        return;
    };
    let Some(next) = next else {
        out.inner.diffs_cell.release_owned(follow.guard_key);
        apply_row_diff(&out, follow, generation, MapDiff::Initial {
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
            apply_row_diff(&out, &follow, generation, diff.clone());
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

/// Apply a row diff to a collection reader in the reader's fire, if the
/// attachment that told it is live there.
fn apply_row_diff<K, RK, V>(
    out: &CellMap<RK, V, CellMutable>,
    follow: &Arc<Follow<K>>,
    generation: u64,
    diff: MapDiff<RK, V>,
) where
    K: Send + Sync + 'static,
    RK: CellValue + Hash + Eq,
    V: CellValue,
{
    let follow = Arc::clone(follow);
    let target = out.downgrade();
    let reader: Arc<dyn Node> = out.inner.diffs_cell.inner.clone();
    crate::settle::run_in_fire(
        &reader,
        Box::new(move || {
            if follow.live(generation)
                && let Some(out) = target.upgrade()
            {
                out.apply_diff_owned(diff);
            }
        }),
    );
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
    value: Option<Arc<T>>,
}

/// A set join's members. `held` is the family's view, changed in its fire:
/// each key's attachment and the guard that holds its row. `rows` is the
/// reader's view, changed in the reader's fire at the same ticket.
struct Members<K, T> {
    held: Mutex<IndexMap<K, (u64, Uuid)>>,
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
            held: Mutex::new(IndexMap::new()),
            rows: Mutex::new(IndexMap::new()),
            attaches: AtomicU64::new(0),
        });
        let node = family.node();
        out.own(node.gate(&out.inner.core, &self));
        let weak = out.downgrade();
        let builder = node.clone();
        let key_guard = self.subscribe(move |signal| {
            if let Signal::Value(keys) = signal {
                builder.build(|| switch_set(&weak, &members, &family, &**keys));
            }
        });
        out.own(node.hold(key_guard));
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
    let reader: Arc<dyn Node> = out.inner.clone();
    let (left, entered, order) = {
        let mut held = members.held.lock();
        // Rebuild in the set's order, keeping members that stay.
        let mut next = IndexMap::with_capacity(held.len());
        let mut entered = Vec::new();
        for key in keys.keys() {
            if next.contains_key(key) {
                continue;
            }
            if let Some(member) = held.swap_remove(key) {
                next.insert(key.clone(), member);
                continue;
            }
            let attach = members.attaches.fetch_add(1, Ordering::Relaxed);
            let guard_key = Uuid::new_v4();
            next.insert(key.clone(), (attach, guard_key));
            entered.push((key.clone(), attach, guard_key));
        }
        let order: Vec<(K, u64)> = next.iter().map(|(k, (a, _))| (k.clone(), *a)).collect();
        let left: Vec<Uuid> = std::mem::replace(&mut *held, next)
            .into_values()
            .map(|(_, guard_key)| guard_key)
            .collect();
        drop(held);
        (left, entered, order)
    };
    // The reader takes the new membership in its own fire at this ticket,
    // ahead of the new rows' replays, and after the deliveries it still owes
    // at earlier tickets.
    {
        let members = Arc::clone(members);
        let target = out.downgrade();
        crate::settle::run_in_fire(
            &reader,
            Box::new(move || {
                let Some(out) = target.upgrade() else { return };
                let mut rows = members.rows.lock();
                let mut old = std::mem::take(&mut *rows);
                for (key, attach) in order {
                    let member = old
                        .swap_remove(&key)
                        .filter(|member| member.attach == attach)
                        .unwrap_or(Member {
                            attach,
                            value: None,
                        });
                    rows.insert(key, member);
                }
                let snapshot = Members::snapshot(&rows);
                drop(rows);
                out.set(snapshot);
            }),
        );
    }
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
                let target = out.downgrade();
                let reader: Arc<dyn Node> = out.inner.clone();
                // Applied in the reader's fire: a replay in the family's fire
                // must not snapshot ahead of a row update the reader applies
                // in the same ticket.
                crate::settle::run_in_fire(
                    &reader,
                    Box::new(move || {
                        let Some(out) = target.upgrade() else { return };
                        let mut rows = members.rows.lock();
                        let Some(member) = rows.get_mut(&key).filter(|m| m.attach == attach)
                        else {
                            return;
                        };
                        member.value = Some(value);
                        let snapshot = Members::snapshot(&rows);
                        drop(rows);
                        out.set(snapshot);
                    }),
                );
            }
        };
        let guard = lease.row().subscribe(deliver);
        let mut lease = Some(lease);
        out.own_keyed(guard_key, guard.with_cleanup(move || drop(lease.take())));
    }
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
        let source = self.materialize();
        let out = CellMap::<K, T, CellMutable>::new();
        let node = family.node();
        out.inner
            .diffs_cell
            .own(node.gate(&out.inner.diffs_cell.inner.core, &source.inner.diffs_cell));
        let each = Arc::new(Each {
            entries: Mutex::new(HashMap::new()),
            live: Mutex::new(HashMap::new()),
            attaches: AtomicU64::new(0),
        });
        let weak = out.downgrade();
        let builder = node.clone();
        let guard = source.subscribe_diffs(move |diff| {
            builder.build(|| each_diff(&weak, &each, &family, &key, diff));
        });
        out.own_guard(node.hold(guard));
        out.lock()
    }
}

/// The entries [`LookupEachExt::lookup_each`] joins: each source key's
/// family key and the attachment that may still deliver for it.
struct Each<K, FK> {
    /// The family's view, changed in its fire.
    entries: Mutex<HashMap<K, Joined<FK>>>,
    /// The reader's view, changed in the reader's fire at the same ticket:
    /// the attachment that may deliver for each entry.
    live: Mutex<HashMap<K, u64>>,
    attaches: AtomicU64,
}

struct Joined<FK> {
    family_key: FK,
    attach: u64,
    guard_key: Uuid,
}

/// Apply one source diff in the family's fire: lease the row of each new or
/// rekeyed entry, and release the rows of entries that left or rekeyed.
fn each_diff<F, K, V, T, RM, KF>(
    weak: &WeakCellMap<K, T>,
    each: &Arc<Each<K, F::Key>>,
    family: &F,
    key: &KF,
    diff: &MapDiff<K, V>,
) where
    F: Family<Row = Cell<T, RM>>,
    K: CellValue + Hash + Eq,
    V: CellValue,
    T: CellValue,
    RM: Send + Sync + 'static,
    KF: Fn(&K, &V) -> F::Key,
{
    let Some(out) = weak.upgrade() else { return };
    match diff {
        MapDiff::Initial { entries } => {
            let stale: Vec<K> = {
                let present: std::collections::HashSet<&K> = entries.iter().map(|(k, _)| k).collect();
                each.entries
                    .lock()
                    .keys()
                    .filter(|k| !present.contains(k))
                    .cloned()
                    .collect()
            };
            for k in stale {
                leave(&out, each, &k);
            }
            for (k, v) in entries {
                join_entry(&out, each, family, k, key(k, v));
            }
        }
        MapDiff::Insert { key: k, value } | MapDiff::Update { key: k, new_value: value, .. } => {
            join_entry(&out, each, family, k, key(k, value));
        }
        MapDiff::Remove { key: k, .. } => leave(&out, each, k),
        MapDiff::Batch { changes } => {
            for change in changes {
                each_diff(weak, each, family, key, change);
            }
        }
    }
}

fn join_entry<F, K, T, RM>(
    out: &CellMap<K, T, CellMutable>,
    each: &Arc<Each<K, F::Key>>,
    family: &F,
    k: &K,
    family_key: F::Key,
) where
    F: Family<Row = Cell<T, RM>>,
    K: CellValue + Hash + Eq,
    T: CellValue,
    RM: Send + Sync + 'static,
{
    let attach = each.attaches.fetch_add(1, Ordering::Relaxed);
    let guard_key = Uuid::new_v4();
    let replaced = {
        let mut entries = each.entries.lock();
        if entries.get(k).is_some_and(|joined| joined.family_key == family_key) {
            return;
        }
        entries.insert(
            k.clone(),
            Joined {
                family_key: family_key.clone(),
                attach,
                guard_key,
            },
        )
    };
    if let Some(replaced) = replaced {
        out.inner.diffs_cell.release_owned(replaced.guard_key);
    }
    let reader: Arc<dyn Node> = out.inner.diffs_cell.inner.clone();
    {
        let each = Arc::clone(each);
        let k = k.clone();
        crate::settle::run_in_fire(
            &reader,
            Box::new(move || {
                each.live.lock().insert(k, attach);
            }),
        );
    }
    let lease = family.acquire(&family_key);
    let deliver = {
        let weak = out.downgrade();
        let each = Arc::clone(each);
        let k = k.clone();
        let reader = Arc::clone(&reader);
        move |signal: &Signal<T>| {
            let Signal::Value(value) = signal else { return };
            let value = Arc::clone(value);
            let Some(out) = weak.upgrade() else { return };
            let weak = out.downgrade();
            let each = Arc::clone(&each);
            let k = k.clone();
            crate::settle::run_in_fire(
                &reader,
                Box::new(move || {
                    let live = each.live.lock().get(&k) == Some(&attach);
                    if live && let Some(out) = weak.upgrade() {
                        out.insert(k, T::clone(&value));
                    }
                }),
            );
        }
    };
    let guard = lease.row().subscribe(deliver);
    let mut lease = Some(lease);
    out.inner
        .diffs_cell
        .own_keyed(guard_key, guard.with_cleanup(move || drop(lease.take())));
}

fn leave<K, T, FK>(out: &CellMap<K, T, CellMutable>, each: &Arc<Each<K, FK>>, k: &K)
where
    K: CellValue + Hash + Eq,
    T: CellValue,
    FK: Send + Sync + 'static,
{
    let left = each.entries.lock().remove(k);
    if let Some(left) = left {
        out.inner.diffs_cell.release_owned(left.guard_key);
        let reader: Arc<dyn Node> = out.inner.diffs_cell.inner.clone();
        let weak = out.downgrade();
        let k = k.clone();
        let attach = left.attach;
        let each = Arc::clone(each);
        crate::settle::run_in_fire(
            &reader,
            Box::new(move || {
                let mut live = each.live.lock();
                if live.get(&k) == Some(&attach) {
                    live.remove(&k);
                    drop(live);
                    if let Some(out) = weak.upgrade() {
                        out.remove(&k);
                    }
                }
            }),
        );
    }
}

impl<K, V, Q> LookupEachExt<K, V> for Q
where
    K: CellValue + Hash + Eq,
    V: CellValue,
    Q: MapQuery<Key = K, Value = V>,
{
}
