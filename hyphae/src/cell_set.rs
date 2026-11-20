//! Reactive `HashSet` with membership observability.
//!
//! `CellSet` wraps a concurrent `HashSet` where membership changes can be observed.

use std::{cell::RefCell, hash::Hash, marker::PhantomData, sync::Arc};

use dashmap::DashSet;

use crate::{
    cell::{Cell, CellImmutable, CellMutable, WeakCell},
    cell_map::{InFlight, WriteOrder, joins_settle, write_in_order},
    pipeline::{Definite, Materialize},
    signal::Signal,
    traits::{CellValue, Gettable, Mutable, Watchable},
};

/// Diff notification for set changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SetDiff<T> {
    /// A value was inserted.
    Insert(T),
    /// A value was removed.
    Remove(T),
}

struct CellSetInner<T>
where
    T: Hash + Eq + CellValue,
{
    /// The actual data storage.
    data: DashSet<T>,
    /// Cached per-value observation cells.
    membership_cells: dashmap::DashMap<T, WeakCell<bool, CellMutable>>,
    /// Cell for diff notifications.
    diffs_cell: Cell<Option<SetDiff<T>>, CellMutable>,
    /// Cell for length.
    len_cell: Cell<usize, CellMutable>,
    /// The write lock, and the values whose membership subscribers have not
    /// been told. A value they hold as a member is recorded as `Some(())`.
    /// See the `publish` module of `cell_map`.
    write_order: WriteOrder<T, ()>,
}

/// A reactive `HashSet` with membership observability.
///
/// # Example
///
/// ```
/// use hyphae::{CellSet, Gettable, Materialize, Watchable, Signal};
///
/// let set = CellSet::<String>::new();
///
/// // Observe membership of a specific value
/// let is_member = set.contains(&"admin".to_string()).materialize();
/// assert_eq!(is_member.get(), false);
///
/// // Insert triggers update
/// set.insert("admin".to_string());
/// assert_eq!(is_member.get(), true);
///
/// // Observe all values
/// let values = set.values().materialize();
/// assert_eq!(values.get().len(), 1);
/// ```
pub struct CellSet<T, M = CellMutable>
where
    T: Hash + Eq + CellValue,
{
    inner: Arc<CellSetInner<T>>,
    _marker: PhantomData<M>,
}

impl<T> CellSet<T, CellMutable>
where
    T: Hash + Eq + CellValue,
{
    /// Create a new empty `CellSet`.
    #[track_caller]
    pub fn new() -> Self {
        // Diffs are events: a subscriber that accumulates them must see every
        // one, in order, and `SetDiff` has no batch form to fold them into.
        let diffs_cell = Cell::new(None).combine_each();
        Self {
            inner: Arc::new(CellSetInner {
                data: DashSet::new(),
                membership_cells: dashmap::DashMap::new(),
                diffs_cell,
                len_cell: Cell::new(0),
                write_order: InFlight::new(0),
            }),
            _marker: PhantomData,
        }
    }

    /// Insert a value, returning true if it was newly inserted.
    pub fn insert(&self, value: T) -> bool {
        write_in_order(&self.inner.write_order, |flight| {
            let is_new = self.inner.data.insert(value.clone());
            if !is_new {
                return false;
            }
            InFlight::resize(flight, false, true);
            if self.publishes_later(flight, &value, false) {
                return true;
            }
            // Notify membership observers (O(1))
            if let Some(cell) = self.observed_membership_cell(&value) {
                cell.set(true);
            }
            self.inner.diffs_cell.set(Some(SetDiff::Insert(value)));
            self.tell_len(flight);
            true
        })
    }

    /// Remove a value, returning true if it was present.
    pub fn remove(&self, value: &T) -> bool {
        write_in_order(&self.inner.write_order, |flight| {
            let was_present = self.inner.data.remove(value).is_some();
            if !was_present {
                return false;
            }
            InFlight::resize(flight, true, false);
            if self.publishes_later(flight, value, true) {
                return true;
            }
            self.inner
                .diffs_cell
                .set(Some(SetDiff::Remove(value.clone())));
            self.tell_len(flight);

            // Notify membership observers (O(1))
            if let Some(cell) = self.observed_membership_cell(value) {
                cell.set(false);
            }
            true
        })
    }

    /// Decide how a change of `value`'s membership reaches subscribers.
    ///
    /// False when the change joins the settle in progress and no earlier
    /// change of `value` waits to be published: the caller writes the diff
    /// now. Otherwise this queues a publish request and returns true.
    /// `was_member` is the membership before the change.
    fn publishes_later(
        &self,
        flight: &RefCell<InFlight<T, ()>>,
        value: &T,
        was_member: bool,
    ) -> bool {
        if joins_settle(&self.inner.diffs_cell) && !flight.borrow().holds(value) {
            return false;
        }
        let pass = InFlight::begin(flight);
        InFlight::touch(flight, pass, value, was_member.then_some(&()));
        crate::settle::submit_write(
            crate::settle::SettleKey::Shared,
            Box::new(Publish {
                set: Arc::clone(&self.inner),
                value: value.clone(),
            }),
        );
        true
    }

    fn tell_len(&self, flight: &RefCell<InFlight<T, ()>>) {
        if let Some(len) = InFlight::tell_len(flight) {
            self.inner.len_cell.set(len);
        }
    }

    /// Tell subscribers whether `value` is stored, if they hold otherwise.
    /// A settle calls this while it seeds a run.
    fn publish(&self, value: T) {
        let order = self.inner.write_order.lock();
        if !joins_settle(&self.inner.diffs_cell) {
            // The settling thread has a batch of its own open, so a diff
            // written now would wait there and arrive behind later ones. The
            // request waits there instead.
            crate::settle::submit_write(
                crate::settle::SettleKey::Shared,
                Box::new(Publish {
                    set: Arc::clone(&self.inner),
                    value,
                }),
            );
            return;
        }
        let live = self.inner.data.contains(&value).then_some(());
        // A membership cell holds the stored membership from its creation,
        // which can be ahead of what diff subscribers were told.
        if let Some(cell) = self.observed_membership_cell(&value)
            && cell.get() != live.is_some()
        {
            cell.set(live.is_some());
        }
        if InFlight::land(&order, &value, live.as_ref()).is_some() {
            let member = live.is_some();
            self.inner.diffs_cell.set(Some(if member {
                SetDiff::Insert(value)
            } else {
                SetDiff::Remove(value)
            }));
        }
        self.tell_len(&order);
    }

    // The shard guard is released before the cell is written.
    fn observed_membership_cell(&self, value: &T) -> Option<Cell<bool, CellMutable>> {
        let weak = self.inner.membership_cells.get(value)?.value().clone();
        weak.upgrade()
    }

    /// Lock the set to prevent further mutations.
    #[must_use]
    pub fn lock(self) -> CellSet<T, CellImmutable> {
        CellSet {
            inner: self.inner,
            _marker: PhantomData,
        }
    }
}

/// A queued request to tell a set's subscribers whether `value` is stored.
/// Holds the set: a membership cell made while the request waits starts at
/// what subscribers were told, and can outlive every other handle to the set.
struct Publish<T>
where
    T: Hash + Eq + CellValue,
{
    set: Arc<CellSetInner<T>>,
    value: T,
}

impl<T> crate::settle::Write for Publish<T>
where
    T: Hash + Eq + CellValue,
{
    fn apply(self: Box<Self>) {
        CellSet {
            inner: self.set,
            _marker: PhantomData::<CellMutable>,
        }
        .publish(self.value);
    }
}

impl<T> Default for CellSet<T, CellMutable>
where
    T: Hash + Eq + CellValue,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<T, M> CellSet<T, M>
where
    T: Hash + Eq + CellValue,
{
    /// Build an observable pipeline for membership of a specific value.
    ///
    /// Materializing produces a `Cell<bool>` that is `true` when the value is
    /// in the set. Multiple installations currently reuse the same underlying
    /// membership cell, but that cache is not part of the public API.
    #[track_caller]
    pub fn contains(&self, value: &T) -> impl Materialize<bool, Definite> + use<T, M> {
        // Check cache first
        if let Some(weak) = self.inner.membership_cells.get(value)
            && let Some(cell) = weak.upgrade()
        {
            return cell.lock();
        }

        // The cell starts at what subscribers hold, which is the stored
        // membership unless a change of this value waits to be published.
        // Under the write lock no mutation or publish runs between the read
        // and the caching of the cell.
        let order = self.inner.write_order.lock();
        if let Some(weak) = self.inner.membership_cells.get(value)
            && let Some(cell) = weak.upgrade()
        {
            return cell.lock();
        }
        let is_member = self.inner.data.contains(value);
        let cell = Cell::new(is_member);
        let weak = cell.downgrade();

        // Cache it
        self.inner.membership_cells.insert(value.clone(), weak);
        drop(order);

        cell.lock()
    }

    /// Build an observable pipeline of all values.
    ///
    /// Returns a derived cell that maintains its state incrementally via diffs.
    /// The initial call is O(N) to build the snapshot, but subsequent updates
    /// are O(1) as they apply diffs incrementally.
    #[track_caller]
    #[must_use]
    pub fn values(&self) -> impl Materialize<Vec<T>, Definite> + use<T, M> {
        // Build initial values from current data (O(N) once)
        let initial: Vec<T> = self.inner.data.iter().map(|r| r.clone()).collect();

        let cell = Cell::new(initial);
        // Weak cell + strong parent keepalive, with the CELL owning the guard —
        // the same shape every `CellMap` observable uses. The previous form had
        // the *set* own the guard (keyed by a fresh `Uuid` that nothing ever
        // removed) while the closure held a strong clone of the returned cell,
        // so each `values()` call pinned its cell and its subscription for the
        // entire life of the set and went on rebuilding a `Vec` that no longer
        // had a reader.
        let weak_cell = cell.downgrade();
        // Keepalive: an observable must retain its parent map/set, or dropping
        // the set out from under this cell leaves it subscribed to a dead diffs
        // source and silently frozen.
        let set_keepalive = self.inner.clone();

        // Subscribe to diffs and apply incrementally (O(1) per update)
        let first = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let guard = self.inner.diffs_cell.subscribe(move |signal| {
            let _ = &set_keepalive; // hold the parent set alive while this cell lives
            // Skip the initial subscription callback
            if first.swap(false, std::sync::atomic::Ordering::SeqCst) {
                return;
            }
            if let Signal::Value(arc_opt) = signal
                && let Some(diff) = arc_opt.as_ref()
                && let Some(cell) = weak_cell.upgrade()
            {
                let mut values = cell.get();
                match diff {
                    SetDiff::Insert(value) => {
                        values.push(value.clone());
                    }
                    SetDiff::Remove(value) => {
                        values.retain(|v| v != value);
                    }
                }
                cell.set(values);
            }
        });

        cell.own(guard);

        cell.lock()
    }

    /// Build an observable pipeline of the set length.
    #[must_use]
    pub fn len(&self) -> impl Materialize<usize, Definite> + use<T, M> {
        self.inner.len_cell.clone().lock()
    }

    /// Check if set is empty (non-reactive).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inner.data.is_empty()
    }

    /// Build an observable pipeline of diff notifications.
    ///
    /// Emits `Some(SetDiff)` on each insert/remove, starts with `None`.
    #[must_use]
    pub fn diffs(&self) -> impl Materialize<Option<SetDiff<T>>, Definite> + use<T, M> {
        self.inner.diffs_cell.clone().lock()
    }

    /// Check if value exists (non-reactive).
    pub fn contains_value(&self, value: &T) -> bool {
        self.inner.data.contains(value)
    }
}

impl<T, M> Clone for CellSet<T, M>
where
    T: Hash + Eq + CellValue,
{
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            _marker: PhantomData,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::Materialize;
    use crate::traits::{Gettable, Watchable};

    #[test]
    fn test_cellset_basic() {
        let set = CellSet::<String>::new();

        assert!(set.is_empty());
        assert!(!set.contains_value(&"a".to_string()));

        assert!(set.insert("a".to_string())); // newly inserted
        assert!(!set.insert("a".to_string())); // already present
        assert!(set.contains_value(&"a".to_string()));
        assert!(!set.is_empty());

        assert!(set.insert("b".to_string()));
        assert!(set.contains_value(&"b".to_string()));

        assert!(set.remove(&"a".to_string())); // was present
        assert!(!set.remove(&"a".to_string())); // no longer present
        assert!(!set.contains_value(&"a".to_string()));
    }

    #[test]
    fn test_cellset_membership_observation() {
        let set = CellSet::<String>::new();

        // Get cell before value exists
        let is_member = set.contains(&"a".to_string()).materialize();
        assert!(!is_member.get());

        let count = Arc::new(AtomicUsize::new(0));
        let c = count.clone();
        let _guard = is_member.subscribe(move |_| {
            c.fetch_add(1, Ordering::SeqCst);
        });

        assert_eq!(count.load(Ordering::SeqCst), 1); // Initial

        // Insert should trigger update
        set.insert("a".to_string());
        assert!(is_member.get());
        assert_eq!(count.load(Ordering::SeqCst), 2);

        // Duplicate insert should not trigger
        set.insert("a".to_string());
        assert_eq!(count.load(Ordering::SeqCst), 2);

        // Remove should trigger
        set.remove(&"a".to_string());
        assert!(!is_member.get());
        assert_eq!(count.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn test_cellset_values_observation() {
        let set = CellSet::<String>::new();
        let values = set.values().materialize();

        assert_eq!(values.get(), Vec::<String>::new());

        set.insert("a".to_string());
        assert_eq!(values.get().len(), 1);

        set.insert("b".to_string());
        assert_eq!(values.get().len(), 2);

        set.remove(&"a".to_string());
        assert_eq!(values.get().len(), 1);
    }

    /// Dropping a `values()` cell must actually free it. Previously the set
    /// stored the subscription guard itself, keyed by a fresh `Uuid` nothing
    /// ever removed, and the guard's closure held a strong clone of the cell —
    /// so every call leaked a cell plus a live subscription that kept rebuilding
    /// a `Vec` with no reader.
    #[test]
    fn values_cell_is_freed_when_dropped() {
        let set = CellSet::<String>::new();
        let values = set.values().materialize();
        let weak = values.downgrade();
        assert!(weak.upgrade().is_some());

        drop(values);
        assert!(
            weak.upgrade().is_none(),
            "values() cell outlived its last owner — the set is still pinning it"
        );

        // The set itself must remain usable after the observable is gone.
        set.insert("a".to_string());
        assert_eq!(set.len().materialize().get(), 1);
    }

    /// Repeated `values()` calls must not accumulate anything in the set.
    #[test]
    fn repeated_values_calls_do_not_accumulate() {
        let set = CellSet::<String>::new();
        let mut weaks = Vec::new();
        for _ in 0..100 {
            let v = set.values().materialize();
            weaks.push(v.downgrade());
        }
        let live = weaks.iter().filter(|w| w.upgrade().is_some()).count();
        assert_eq!(
            live, 0,
            "{live}/100 values() cells were still pinned after being dropped"
        );
    }

    /// The keepalive half of the invariant: a `values()` cell holds its parent
    /// set alive, so it keeps tracking after the caller's `CellSet` handle is
    /// dropped rather than silently freezing.
    #[test]
    fn values_cell_keeps_parent_set_alive() {
        let values = {
            let set = CellSet::<String>::new();
            set.insert("a".to_string());
            let values = set.values().materialize();
            assert_eq!(values.get().len(), 1);
            values
        };
        // `set` is gone; the observable must still hold a coherent snapshot.
        assert_eq!(values.get().len(), 1);
    }

    #[test]
    fn test_cellset_diffs() {
        let set = CellSet::<String>::new();
        let diffs = set.diffs().materialize();

        assert_eq!(diffs.get(), None);

        set.insert("a".to_string());
        assert_eq!(diffs.get(), Some(SetDiff::Insert("a".to_string())));

        set.remove(&"a".to_string());
        assert_eq!(diffs.get(), Some(SetDiff::Remove("a".to_string())));
    }

    #[test]
    fn test_cellset_len() {
        let set = CellSet::<String>::new();
        let len = set.len().materialize();

        assert_eq!(len.get(), 0);

        set.insert("a".to_string());
        assert_eq!(len.get(), 1);

        set.insert("b".to_string());
        assert_eq!(len.get(), 2);

        set.remove(&"a".to_string());
        assert_eq!(len.get(), 1);
    }

    #[test]
    fn test_cellset_lock() {
        let set = CellSet::<String>::new();
        set.insert("a".to_string());

        let locked = set.lock();

        // Can still observe
        assert!(locked.contains(&"a".to_string()).materialize().get());
        assert_eq!(locked.values().materialize().get().len(), 1);

        // But can't mutate - these methods don't exist on CellImmutable
        // locked.insert(...) // compile error
    }

    #[test]
    fn test_cellset_same_cell_returned() {
        let set = CellSet::<String>::new();

        let cell1 = set.contains(&"a".to_string()).materialize();
        let cell2 = set.contains(&"a".to_string()).materialize();

        // Both should reflect same updates
        set.insert("a".to_string());

        assert!(cell1.get());
        assert!(cell2.get());
    }
}
