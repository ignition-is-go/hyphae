//! Reactive grouped view over a [`CellMap`], indexed by foreign key.
//!
//! `NestedMap<PK, K, V>` takes an underlying `CellMap<K, V>` and partitions its
//! entries by a foreign-key extractor `Fn(&V) -> PK`.  It maintains a live
//! reverse index so:
//!
//! - Subscriptions can be scoped to a single parent key.
//! - Child diffs bubbling up through a tree can be re-tagged with the correct
//!   parent key via [`lookup_parent`](NestedMap::lookup_parent).
//! - It implements [`ReactiveKeys`] and [`ReactiveMap`], so joins compose with
//!   it exactly like they do with `CellMap`.
//!
//! `NestedMap` is **read-only** — writes go to the underlying `CellMap`, and
//! the grouped view updates reactively.

use std::{
    collections::{HashMap, HashSet},
    hash::Hash,
    marker::PhantomData,
    sync::Arc,
};

use arc_swap::ArcSwap;
use dashmap::DashMap;

use crate::{
    cell_map::{CellMap, CellMapInner, MapDiff},
    subscription::SubscriptionGuard,
    traits::{
        CellValue,
        reactive_keys::{KeyChange, ReactiveKeys},
        reactive_map::ReactiveMap,
    },
};

/// Internal index state: FK grouping + reverse lookup.
#[derive(Clone)]
struct NestState<PK, K>
where
    PK: Hash + Eq + CellValue,
    K: Hash + Eq + CellValue,
{
    /// parent_key → { child_keys }
    forward: HashMap<PK, HashSet<K>>,
    /// child_key → parent_key
    reverse: HashMap<K, PK>,
}

impl<PK, K> Default for NestState<PK, K>
where
    PK: Hash + Eq + CellValue,
    K: Hash + Eq + CellValue,
{
    fn default() -> Self {
        Self {
            forward: HashMap::new(),
            reverse: HashMap::new(),
        }
    }
}

/// A reactive grouped view over a `CellMap`, indexed by a foreign key.
///
/// Created via [`CellMap::nest`] (or the planned `TreeNode::join`).
///
/// ```text
/// CellMap<OrderId, Order>
///   └─ NestedMap<CustomerId, OrderId, Order>
///        ├── "cust_1" → { "ord_1", "ord_3" }
///        └── "cust_2" → { "ord_2" }
/// ```
pub struct NestedMap<PK, K, V>
where
    PK: Hash + Eq + CellValue,
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    /// The underlying source data (M-erased: CellMapInner doesn't vary by M).
    source_inner: Arc<CellMapInner<K, V>>,
    /// Live FK index, updated reactively.
    state: Arc<ArcSwap<NestState<PK, K>>>,
    /// For concurrent parent-key lookups.
    reverse_cache: Arc<DashMap<K, PK>>,
    /// Subscription guard keeping the index alive.
    _index_guard: Arc<SubscriptionGuard>,
    _marker: PhantomData<V>,
}

impl<PK, K, V> Clone for NestedMap<PK, K, V>
where
    PK: Hash + Eq + CellValue,
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    fn clone(&self) -> Self {
        Self {
            source_inner: self.source_inner.clone(),
            state: self.state.clone(),
            reverse_cache: self.reverse_cache.clone(),
            _index_guard: self._index_guard.clone(),
            _marker: PhantomData,
        }
    }
}

impl<PK, K, V> NestedMap<PK, K, V>
where
    PK: Hash + Eq + CellValue,
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    /// Create a new `NestedMap` by grouping `source` entries via `fk`.
    pub fn new<M: Send + Sync + 'static>(
        source: &CellMap<K, V, M>,
        fk: impl Fn(&V) -> PK + Send + Sync + 'static,
    ) -> Self {
        let state = Arc::new(ArcSwap::from_pointee(NestState::default()));
        let reverse_cache = Arc::new(DashMap::<K, PK>::new());

        let state_for_sub = state.clone();
        let reverse_cache_for_sub = reverse_cache.clone();
        let fk = Arc::new(fk);

        let guard = source.subscribe_diffs(move |diff| {
            fn apply<PK, K, V>(
                st: &mut NestState<PK, K>,
                rc: &DashMap<K, PK>,
                diff: &MapDiff<K, V>,
                fk: &dyn Fn(&V) -> PK,
            ) where
                PK: Hash + Eq + CellValue,
                K: Hash + Eq + CellValue,
                V: CellValue,
            {
                match diff {
                    MapDiff::Initial { entries } => {
                        st.forward.clear();
                        st.reverse.clear();
                        rc.clear();
                        for (k, v) in entries {
                            let pk = fk(v);
                            st.forward.entry(pk.clone()).or_default().insert(k.clone());
                            st.reverse.insert(k.clone(), pk.clone());
                            rc.insert(k.clone(), pk);
                        }
                    }
                    MapDiff::Insert { key, value } => {
                        let pk = fk(value);
                        st.forward
                            .entry(pk.clone())
                            .or_default()
                            .insert(key.clone());
                        st.reverse.insert(key.clone(), pk.clone());
                        rc.insert(key.clone(), pk);
                    }
                    MapDiff::Remove { key, .. } => {
                        if let Some(old_pk) = st.reverse.remove(key)
                            && let Some(set) = st.forward.get_mut(&old_pk)
                        {
                            set.remove(key);
                            if set.is_empty() {
                                st.forward.remove(&old_pk);
                            }
                        }
                        rc.remove(key);
                    }
                    MapDiff::Update { key, new_value, .. } => {
                        let new_pk = fk(new_value);
                        // Remove from old group
                        if let Some(old_pk) = st.reverse.insert(key.clone(), new_pk.clone())
                            && old_pk != new_pk
                            && let Some(set) = st.forward.get_mut(&old_pk)
                        {
                            set.remove(key);
                            if set.is_empty() {
                                st.forward.remove(&old_pk);
                            }
                        }
                        st.forward
                            .entry(new_pk.clone())
                            .or_default()
                            .insert(key.clone());
                        rc.insert(key.clone(), new_pk);
                    }
                    MapDiff::Batch { changes } => {
                        for change in changes {
                            apply(st, rc, change, fk);
                        }
                    }
                }
            }

            state_for_sub.rcu(|current| {
                let mut next = current.as_ref().clone();
                apply(&mut next, &reverse_cache_for_sub, diff, fk.as_ref());
                next
            });
        });

        Self {
            source_inner: source.inner.clone(),
            state,
            reverse_cache,
            _index_guard: Arc::new(guard),
            _marker: PhantomData,
        }
    }

    /// Look up the parent key for a given child key.
    ///
    /// This is the critical operation for tree diff routing: when a child diff
    /// bubbles up, the parent node calls `lookup_parent` to re-tag it with the
    /// correct parent key.
    pub fn lookup_parent(&self, child_key: &K) -> Option<PK> {
        self.reverse_cache.get(child_key).map(|r| r.value().clone())
    }

    /// Get all child keys currently grouped under `parent_key`.
    pub fn children_of(&self, parent_key: &PK) -> Vec<K> {
        let snapshot = self.state.load();
        snapshot
            .forward
            .get(parent_key)
            .map(|set| set.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Subscribe to the underlying source's diffs (initial + subsequent).
    fn subscribe_source_diffs(
        &self,
        cb: impl Fn(&MapDiff<K, V>) + Send + Sync + 'static,
    ) -> SubscriptionGuard {
        use crate::traits::Watchable;

        // Emit initial snapshot
        let entries: Vec<(K, V)> = self
            .source_inner
            .data
            .iter()
            .map(|r| (r.key().clone(), r.value().clone()))
            .collect();
        cb(&MapDiff::Initial { entries });

        // Subscribe to subsequent diffs
        let keepalive = self.source_inner.clone();
        let diffs = self.source_inner.diffs_cell.clone().lock();
        let first = Arc::new(std::sync::atomic::AtomicBool::new(true));
        diffs.subscribe(move |signal| {
            let _ = &keepalive;
            if first.swap(false, std::sync::atomic::Ordering::SeqCst) {
                return;
            }
            if let crate::Signal::Value(diff) = signal {
                cb(diff.as_ref());
            }
        })
    }

    /// Subscribe to diffs scoped to a single parent key.
    ///
    /// Only diffs affecting children of `parent_key` are forwarded.
    pub fn subscribe_parent(
        &self,
        parent_key: PK,
        cb: impl Fn(&MapDiff<K, V>) + Send + Sync + 'static,
    ) -> SubscriptionGuard {
        let reverse_cache = self.reverse_cache.clone();
        self.subscribe_source_diffs(move |diff| {
            fn filter_for_parent<PK, K, V>(
                diff: &MapDiff<K, V>,
                _parent_key: &PK,
                _rc: &DashMap<K, PK>,
                fk_match: &dyn Fn(&K) -> bool,
            ) -> Option<MapDiff<K, V>>
            where
                PK: Hash + Eq + CellValue,
                K: Hash + Eq + CellValue,
                V: CellValue,
            {
                match diff {
                    MapDiff::Initial { entries } => {
                        let filtered: Vec<_> = entries
                            .iter()
                            .filter(|(k, _)| fk_match(k))
                            .cloned()
                            .collect();
                        if filtered.is_empty() {
                            None
                        } else {
                            Some(MapDiff::Initial { entries: filtered })
                        }
                    }
                    MapDiff::Insert { key, .. }
                    | MapDiff::Remove { key, .. }
                    | MapDiff::Update { key, .. } => {
                        if fk_match(key) {
                            Some(diff.clone())
                        } else {
                            None
                        }
                    }
                    MapDiff::Batch { changes } => {
                        let filtered: Vec<_> = changes
                            .iter()
                            .filter_map(|c| filter_for_parent(c, _parent_key, _rc, fk_match))
                            .collect();
                        if filtered.is_empty() {
                            None
                        } else {
                            Some(MapDiff::Batch { changes: filtered })
                        }
                    }
                }
            }

            let pk = parent_key.clone();
            let fk_match =
                |k: &K| -> bool { reverse_cache.get(k).is_some_and(|r| *r.value() == pk) };

            if let Some(filtered) = filter_for_parent(diff, &parent_key, &reverse_cache, &fk_match)
            {
                cb(&filtered);
            }
        })
    }

    /// Subscribe to all diffs, tagged with the parent key.
    ///
    /// The callback receives `(parent_key, child_diff)` for every change.
    /// This is used by the tree subscription machinery.
    pub fn subscribe_grouped(
        &self,
        cb: impl Fn(&PK, &MapDiff<K, V>) + Send + Sync + 'static,
    ) -> SubscriptionGuard {
        let reverse_cache = self.reverse_cache.clone();
        let fk_arc = self.state.clone();
        self.subscribe_source_diffs(move |diff| {
            fn route_diff<PK, K, V>(
                diff: &MapDiff<K, V>,
                rc: &DashMap<K, PK>,
                cb: &dyn Fn(&PK, &MapDiff<K, V>),
            ) where
                PK: Hash + Eq + CellValue,
                K: Hash + Eq + CellValue,
                V: CellValue,
            {
                match diff {
                    MapDiff::Initial { entries } => {
                        // Group entries by parent key, emit one Initial per group.
                        let mut groups: HashMap<PK, Vec<(K, V)>> = HashMap::new();
                        for (k, v) in entries {
                            if let Some(pk) = rc.get(k) {
                                groups
                                    .entry(pk.value().clone())
                                    .or_default()
                                    .push((k.clone(), v.clone()));
                            }
                        }
                        for (pk, group_entries) in groups {
                            cb(
                                &pk,
                                &MapDiff::Initial {
                                    entries: group_entries,
                                },
                            );
                        }
                    }
                    MapDiff::Insert { key, .. }
                    | MapDiff::Remove { key, .. }
                    | MapDiff::Update { key, .. } => {
                        if let Some(pk) = rc.get(key) {
                            cb(pk.value(), diff);
                        }
                    }
                    MapDiff::Batch { changes } => {
                        // Group batch members by parent key, emit one Batch per group.
                        let mut groups: HashMap<PK, Vec<MapDiff<K, V>>> = HashMap::new();
                        fn collect_by_parent<PK, K, V>(
                            diff: &MapDiff<K, V>,
                            rc: &DashMap<K, PK>,
                            groups: &mut HashMap<PK, Vec<MapDiff<K, V>>>,
                        ) where
                            PK: Hash + Eq + CellValue,
                            K: Hash + Eq + CellValue,
                            V: CellValue,
                        {
                            match diff {
                                MapDiff::Insert { key, .. }
                                | MapDiff::Remove { key, .. }
                                | MapDiff::Update { key, .. } => {
                                    if let Some(pk) = rc.get(key) {
                                        groups
                                            .entry(pk.value().clone())
                                            .or_default()
                                            .push(diff.clone());
                                    }
                                }
                                MapDiff::Batch { changes } => {
                                    for change in changes {
                                        collect_by_parent(change, rc, groups);
                                    }
                                }
                                MapDiff::Initial { .. } => {
                                    // Initial inside a Batch is unusual but handle it
                                    // by treating each entry as an insert.
                                }
                            }
                        }

                        for change in changes {
                            collect_by_parent(change, rc, &mut groups);
                        }
                        for (pk, diffs) in groups {
                            if diffs.len() == 1 {
                                cb(&pk, &diffs[0]);
                            } else {
                                cb(&pk, &MapDiff::Batch { changes: diffs });
                            }
                        }
                    }
                }
            }

            let _ = &fk_arc; // keep state alive
            route_diff(diff, &reverse_cache, &cb);
        })
    }
}

// ── ReactiveKeys ────────────────────────────────────────────────────────

impl<PK, K, V> ReactiveKeys for NestedMap<PK, K, V>
where
    PK: Hash + Eq + CellValue,
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    type Key = K;

    fn key_set(&self) -> Vec<K> {
        self.source_inner
            .data
            .iter()
            .map(|r| r.key().clone())
            .collect()
    }

    fn contains_key(&self, key: &K) -> bool {
        self.source_inner.data.contains_key(key)
    }

    fn subscribe_keys(
        &self,
        cb: impl Fn(&KeyChange<K>) + Send + Sync + 'static,
    ) -> SubscriptionGuard {
        use crate::cell_map::map_diff_to_key_change;
        self.subscribe_source_diffs(move |diff| {
            if let Some(kc) = map_diff_to_key_change(diff) {
                cb(&kc);
            }
        })
    }
}

// ── ReactiveMap ─────────────────────────────────────────────────────────

impl<PK, K, V> ReactiveMap for NestedMap<PK, K, V>
where
    PK: Hash + Eq + CellValue,
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    type Value = V;

    fn get_value(&self, key: &K) -> Option<V> {
        self.source_inner.data.get(key).map(|r| r.value().clone())
    }

    fn snapshot(&self) -> Vec<(K, V)> {
        self.source_inner
            .data
            .iter()
            .map(|r| (r.key().clone(), r.value().clone()))
            .collect()
    }

    fn subscribe_diffs_reactive(
        &self,
        cb: impl Fn(&MapDiff<K, V>) + Send + Sync + 'static,
    ) -> SubscriptionGuard {
        self.subscribe_source_diffs(cb)
    }
}

// ── Constructor on CellMap ──────────────────────────────────────────────

impl<K, V, M> CellMap<K, V, M>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
    M: Send + Sync + 'static,
{
    /// Create a grouped view of this map, partitioned by a foreign-key
    /// extractor.
    ///
    /// The returned `NestedMap` observes this `CellMap` reactively and
    /// maintains a live FK index. It implements [`ReactiveKeys`] and
    /// [`ReactiveMap`], so it composes with joins exactly like a `CellMap`.
    ///
    /// ```text
    /// let orders: CellMap<OrderId, Order> = CellMap::new();
    /// let nested = orders.nest(|order| order.customer_id.clone());
    /// // nested: NestedMap<CustomerId, OrderId, Order>
    /// ```
    pub fn nest<PK>(&self, fk: impl Fn(&V) -> PK + Send + Sync + 'static) -> NestedMap<PK, K, V>
    where
        PK: Hash + Eq + CellValue,
    {
        NestedMap::new(self, fk)
    }
}
