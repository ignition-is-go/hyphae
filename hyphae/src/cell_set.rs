//! Reactive HashSet with membership observability.
//!
//! `CellSet` wraps a concurrent HashSet where membership changes can be observed.

use std::{hash::Hash, marker::PhantomData, sync::Arc};

use dashmap::DashSet;
use uuid::Uuid;

use crate::{
    cell::{Cell, CellImmutable, CellMutable, WeakCell},
    signal::Signal,
    subscription::SubscriptionGuard,
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
    /// Subscription guards owned by this set (dropped when set drops).
    owned: dashmap::DashMap<Uuid, SubscriptionGuard>,
}

/// A reactive HashSet with membership observability.
///
/// # Example
///
/// ```
/// use hyphae::{CellSet, Gettable, Watchable, Signal};
///
/// let set = CellSet::<String>::new();
///
/// // Observe membership of a specific value
/// let is_member = set.contains(&"admin".to_string());
/// assert_eq!(is_member.get(), false);
///
/// // Insert triggers update
/// set.insert("admin".to_string());
/// assert_eq!(is_member.get(), true);
///
/// // Observe all values
/// let values = set.values();
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
    /// Create a new empty CellSet.
    #[track_caller]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(CellSetInner {
                data: DashSet::new(),
                membership_cells: dashmap::DashMap::new(),
                diffs_cell: Cell::new(None),
                len_cell: Cell::new(0),
                owned: dashmap::DashMap::new(),
            }),
            _marker: PhantomData,
        }
    }

    /// Insert a value, returning true if it was newly inserted.
    pub fn insert(&self, value: T) -> bool {
        let is_new = self.inner.data.insert(value.clone());

        if is_new {
            // Emit diff (O(1) - just notifies subscribers)
            self.inner
                .diffs_cell
                .set(Some(SetDiff::Insert(value.clone())));

            // Update len (O(1))
            self.inner.len_cell.set(self.inner.data.len());

            // Notify membership observers (O(1))
            if let Some(weak) = self.inner.membership_cells.get(&value)
                && let Some(cell) = weak.upgrade()
            {
                cell.set(true);
            }
        }

        is_new
    }

    /// Remove a value, returning true if it was present.
    pub fn remove(&self, value: &T) -> bool {
        let was_present = self.inner.data.remove(value).is_some();

        if was_present {
            // Emit diff (O(1) - just notifies subscribers)
            self.inner
                .diffs_cell
                .set(Some(SetDiff::Remove(value.clone())));

            // Update len (O(1))
            self.inner.len_cell.set(self.inner.data.len());

            // Notify membership observers (O(1))
            if let Some(weak) = self.inner.membership_cells.get(value)
                && let Some(cell) = weak.upgrade()
            {
                cell.set(false);
            }
        }

        was_present
    }

    /// Lock the set to prevent further mutations.
    pub fn lock(self) -> CellSet<T, CellImmutable> {
        CellSet {
            inner: self.inner,
            _marker: PhantomData,
        }
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
    /// Get an observable Cell for membership of a specific value.
    ///
    /// Returns a `Cell<bool>` that is `true` when the value is in the set.
    /// Multiple calls with the same value return the same underlying Cell.
    #[track_caller]
    pub fn contains(&self, value: &T) -> Cell<bool, CellImmutable> {
        // Check cache first
        if let Some(weak) = self.inner.membership_cells.get(value)
            && let Some(cell) = weak.upgrade()
        {
            return cell.lock();
        }

        // Create new cell with current membership status
        let is_member = self.inner.data.contains(value);
        let cell = Cell::new(is_member);
        let weak = cell.downgrade();

        // Cache it
        self.inner.membership_cells.insert(value.clone(), weak);

        cell.lock()
    }

    /// Get an observable Cell of all values.
    ///
    /// Returns a derived cell that maintains its state incrementally via diffs.
    /// The initial call is O(N) to build the snapshot, but subsequent updates
    /// are O(1) as they apply diffs incrementally.
    #[track_caller]
    pub fn values(&self) -> Cell<Vec<T>, CellImmutable> {
        // Build initial values from current data (O(N) once)
        let initial: Vec<T> = self.inner.data.iter().map(|r| r.clone()).collect();

        let cell = Cell::new(initial);
        let cell_clone = cell.clone();

        // Subscribe to diffs and apply incrementally (O(1) per update)
        let first = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        let guard = self.inner.diffs_cell.subscribe(move |signal| {
            // Skip the initial subscription callback
            if first.swap(false, std::sync::atomic::Ordering::SeqCst) {
                return;
            }
            if let Signal::Value(arc_opt) = signal
                && let Some(diff) = arc_opt.as_ref()
            {
                let mut values = cell_clone.get();
                match diff {
                    SetDiff::Insert(value) => {
                        values.push(value.clone());
                    }
                    SetDiff::Remove(value) => {
                        values.retain(|v| v != value);
                    }
                }
                cell_clone.set(values);
            }
        });

        // Store the guard so it lives as long as the set
        self.inner.owned.insert(Uuid::new_v4(), guard);

        cell.lock()
    }

    /// Get an observable Cell of the set length.
    pub fn len(&self) -> Cell<usize, CellImmutable> {
        self.inner.len_cell.clone().lock()
    }

    /// Check if set is empty (non-reactive).
    pub fn is_empty(&self) -> bool {
        self.inner.data.is_empty()
    }

    /// Get an observable Cell of diff notifications.
    ///
    /// Emits `Some(SetDiff)` on each insert/remove, starts with `None`.
    pub fn diffs(&self) -> Cell<Option<SetDiff<T>>, CellImmutable> {
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
        let is_member = set.contains(&"a".to_string());
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
        let values = set.values();

        assert_eq!(values.get(), Vec::<String>::new());

        set.insert("a".to_string());
        assert_eq!(values.get().len(), 1);

        set.insert("b".to_string());
        assert_eq!(values.get().len(), 2);

        set.remove(&"a".to_string());
        assert_eq!(values.get().len(), 1);
    }

    #[test]
    fn test_cellset_diffs() {
        let set = CellSet::<String>::new();
        let diffs = set.diffs();

        assert_eq!(diffs.get(), None);

        set.insert("a".to_string());
        assert_eq!(diffs.get(), Some(SetDiff::Insert("a".to_string())));

        set.remove(&"a".to_string());
        assert_eq!(diffs.get(), Some(SetDiff::Remove("a".to_string())));
    }

    #[test]
    fn test_cellset_len() {
        let set = CellSet::<String>::new();
        let len = set.len();

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
        assert!(locked.contains(&"a".to_string()).get());
        assert_eq!(locked.values().get().len(), 1);

        // But can't mutate - these methods don't exist on CellImmutable
        // locked.insert(...) // compile error
    }

    #[test]
    fn test_cellset_same_cell_returned() {
        let set = CellSet::<String>::new();

        let cell1 = set.contains(&"a".to_string());
        let cell2 = set.contains(&"a".to_string());

        // Both should reflect same updates
        set.insert("a".to_string());

        assert!(cell1.get());
        assert!(cell2.get());
    }
}
