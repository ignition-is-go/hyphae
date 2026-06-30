//! Bridge hyphae `CellMap`/`NestedMap` into fine-grained Leptos reactive state.
//!
//! Rather than one coarse `RwSignal<HashMap<..>>` (where any change wakes every
//! reader), each key owns its own `RwSignal<Option<V>>`. A `MapDiff::Update` to
//! one key re-runs only the views reading that key — the same fine-grained
//! behaviour Leptos `reactive_stores` give a static struct, applied to a
//! dynamic, diff-driven collection.
//!
//! Per-key signals are `Option`-wrapped to support *subscribe-before-data*:
//! [`MapGroup::value`] always hands back a signal — `None` until the key's first
//! diff lands — mirroring hyphae's own [`hyphae::CellMap::get`], which returns a
//! `Cell<Option<V>>` for a key that may not exist yet.

use std::{collections::HashMap, hash::Hash, ops::Deref};

use hyphae::{CellMap, CellValue, MapDiff, NestedMap, SubscriptionGuard};
use leptos::prelude::{
    Dispose, GetUntracked, ReadSignal, RwSignal, Set, Signal, StoredValue, Update, UpdateValue,
    WithUntracked, WithValue, on_cleanup,
};

/// A keyed collection of per-key reactive value signals plus a reactive key
/// list. This is the shared core of [`CellMapStore`] and each group inside a
/// [`NestedMapStore`].
pub struct MapGroup<K, V>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    /// Reactive key list — changes on structural edits (insert/remove). Drive a
    /// Leptos `<For>` from this.
    keys: RwSignal<Vec<K>>,
    /// Per-key value signals, `Option`-wrapped for subscribe-before-data. Held
    /// in a `StoredValue` so the map is owned by the reactive arena and
    /// `Copy`/`Send`/`Sync` for use in subscription closures.
    values: StoredValue<HashMap<K, RwSignal<Option<V>>>>,
}

impl<K, V> Clone for MapGroup<K, V>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    fn clone(&self) -> Self {
        *self
    }
}

impl<K, V> Copy for MapGroup<K, V>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
}

impl<K, V> MapGroup<K, V>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    fn new() -> Self {
        Self {
            keys: RwSignal::new(Vec::new()),
            values: StoredValue::new(HashMap::new()),
        }
    }

    /// Reactive list of keys. Use with Leptos `<For>` to render one view per
    /// entry; it updates only on inserts/removals, not value changes.
    pub fn keys(&self) -> Signal<Vec<K>> {
        self.keys.into()
    }

    /// The reactive value signal for `key` — `None` until the key's first diff
    /// lands (*subscribe-before-data*), then `Some(v)` and tracking every later
    /// change to just this key. Always returns a signal, so a view can bind to a
    /// not-yet-present entity and light up when it arrives.
    pub fn value(&self, key: &K) -> ReadSignal<Option<V>> {
        let existing = self.values.with_value(|m| m.get(key).copied());
        let sig = existing.unwrap_or_else(|| {
            // Create a `None` placeholder so callers can subscribe before data.
            // A later `Insert`/`Update` diff fills it in via `upsert`.
            let sig = RwSignal::new(None);
            self.values.update_value(|m| {
                m.insert(key.clone(), sig);
            });
            sig
        });
        sig.read_only()
    }

    /// Non-reactive snapshot of a key's current value (`None` if absent).
    pub fn get(&self, key: &K) -> Option<V> {
        self.values
            .with_value(|m| m.get(key).and_then(|s| s.get_untracked()))
    }

    /// Whether `key` currently exists. Reflects live entries (the key list), not
    /// placeholder signals created by [`value`](Self::value) ahead of data.
    pub fn contains_key(&self, key: &K) -> bool {
        self.keys.with_untracked(|ks| ks.contains(key))
    }

    /// Number of live entries (non-reactive).
    pub fn len(&self) -> usize {
        self.keys.with_untracked(Vec::len)
    }

    /// Whether the group is empty (non-reactive).
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Apply one diff, creating/updating/removing per-key signals so only the
    /// affected keys notify.
    fn apply(&self, diff: &MapDiff<K, V>) {
        match diff {
            MapDiff::Initial { entries } => {
                // Seed each entry into its (possibly pre-existing placeholder)
                // signal so subscribe-before-data holders light up.
                for (k, v) in entries {
                    self.upsert(k, v);
                }
                self.keys.set(entries.iter().map(|(k, _)| k.clone()).collect());
            }
            MapDiff::Insert { key, value } => self.upsert(key, value),
            MapDiff::Update {
                key, new_value, ..
            } => self.upsert(key, new_value),
            MapDiff::Remove { key, .. } => {
                self.keys.update(|ks| ks.retain(|k| k != key));
                // Keep the signal (set to `None`) rather than disposing it: a
                // view may still hold it (subscribe-before-data), and a later
                // re-insert of the same key must refill the *same* signal.
                self.values.update_value(|m| {
                    if let Some(sig) = m.get(key) {
                        sig.set(None);
                    }
                });
            }
            MapDiff::Batch { changes } => {
                for change in changes {
                    self.apply(change);
                }
            }
        }
    }

    /// Set a key's value (fine-grained), creating its signal if absent and
    /// extending the key list if the key isn't already live.
    fn upsert(&self, key: &K, value: &V) {
        self.values.update_value(|m| match m.get(key) {
            Some(sig) => sig.set(Some(value.clone())),
            None => {
                m.insert(key.clone(), RwSignal::new(Some(value.clone())));
            }
        });
        // Newness is tracked against the live key list, not the signal map: a
        // placeholder (from `value`) or a tombstone (from `Remove`) may already
        // hold a signal for this key without it being a live entry.
        let absent = self.keys.with_untracked(|ks| !ks.contains(key));
        if absent {
            self.keys.update(|ks| ks.push(key.clone()));
        }
    }

    /// Dispose every reactive handle this group owns.
    fn dispose(self) {
        self.values.update_value(|m| {
            for (_, sig) in m.drain() {
                sig.dispose();
            }
        });
        self.values.dispose();
        self.keys.dispose();
    }
}

/// A fine-grained reactive store over a hyphae [`CellMap`].
///
/// Build one with [`CellMapStoreExt::into_leptos_store`]. Derefs to [`MapGroup`]
/// for `keys()` / `value()` access. Call within a reactive owner; the
/// underlying hyphae subscription is released on cleanup.
pub struct CellMapStore<K, V>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    group: MapGroup<K, V>,
    _guard: StoredValue<Option<SubscriptionGuard>>,
}

impl<K, V> Clone for CellMapStore<K, V>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    fn clone(&self) -> Self {
        *self
    }
}

impl<K, V> Copy for CellMapStore<K, V>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
}

impl<K, V> Deref for CellMapStore<K, V>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    type Target = MapGroup<K, V>;

    fn deref(&self) -> &Self::Target {
        &self.group
    }
}

impl<K, V> CellMapStore<K, V>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    fn new<M>(map: &CellMap<K, V, M>) -> Self {
        let group = MapGroup::new();
        // `subscribe_diffs` emits an `Initial` snapshot synchronously, so the
        // group is fully populated before this returns.
        let guard = map.subscribe_diffs(move |diff| group.apply(diff));
        on_cleanup(move || group.dispose());
        Self {
            group,
            _guard: StoredValue::new(Some(guard)),
        }
    }
}

/// A fine-grained reactive store over a hyphae [`NestedMap`]: a reactive list of
/// parent keys, each owning its own [`MapGroup`] of children.
pub struct NestedMapStore<PK, K, V>
where
    PK: Hash + Eq + CellValue,
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    parents: RwSignal<Vec<PK>>,
    groups: StoredValue<HashMap<PK, MapGroup<K, V>>>,
    _guard: StoredValue<Option<SubscriptionGuard>>,
}

impl<PK, K, V> Clone for NestedMapStore<PK, K, V>
where
    PK: Hash + Eq + CellValue,
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    fn clone(&self) -> Self {
        *self
    }
}

impl<PK, K, V> Copy for NestedMapStore<PK, K, V>
where
    PK: Hash + Eq + CellValue,
    K: Hash + Eq + CellValue,
    V: CellValue,
{
}

impl<PK, K, V> NestedMapStore<PK, K, V>
where
    PK: Hash + Eq + CellValue,
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    fn new(nested: &NestedMap<PK, K, V>) -> Self {
        let parents = RwSignal::new(Vec::new());
        let groups: StoredValue<HashMap<PK, MapGroup<K, V>>> = StoredValue::new(HashMap::new());

        let guard = nested.subscribe_grouped(move |pk, diff| {
            // Get-or-create the child group for this parent.
            let (group, is_new) = match groups.with_value(|g| g.get(pk).copied()) {
                Some(group) => (group, false),
                None => {
                    let group = MapGroup::new();
                    groups.update_value(|g| {
                        g.insert(pk.clone(), group);
                    });
                    (group, true)
                }
            };
            if is_new {
                parents.update(|p| p.push(pk.clone()));
            }

            group.apply(diff);

            // Prune a parent once its last child is gone.
            if group.is_empty() {
                groups.update_value(|g| {
                    g.remove(pk);
                });
                parents.update(|p| p.retain(|x| x != pk));
                group.dispose();
            }
        });

        on_cleanup(move || {
            groups.update_value(|g| {
                for (_, group) in g.drain() {
                    group.dispose();
                }
            });
        });

        Self {
            parents,
            groups,
            _guard: StoredValue::new(Some(guard)),
        }
    }

    /// Reactive list of parent keys. Drive a Leptos `<For>` from this to render
    /// one section per parent.
    pub fn parents(&self) -> Signal<Vec<PK>> {
        self.parents.into()
    }

    /// The child [`MapGroup`] for `parent`, if any children are currently
    /// grouped under it.
    pub fn group(&self, parent: &PK) -> Option<MapGroup<K, V>> {
        self.groups.with_value(|g| g.get(parent).copied())
    }
}

/// Extension trait: turn a hyphae [`CellMap`] into a Leptos store.
pub trait CellMapStoreExt<K, V>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    /// Create a fine-grained [`CellMapStore`] driven by this map's diffs.
    fn into_leptos_store(&self) -> CellMapStore<K, V>;
}

impl<K, V, M> CellMapStoreExt<K, V> for CellMap<K, V, M>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    fn into_leptos_store(&self) -> CellMapStore<K, V> {
        CellMapStore::new(self)
    }
}

/// Extension trait: turn a hyphae [`NestedMap`] into a Leptos store.
pub trait NestedMapStoreExt<PK, K, V>
where
    PK: Hash + Eq + CellValue,
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    /// Create a fine-grained [`NestedMapStore`] driven by this nested map.
    fn into_leptos_store(&self) -> NestedMapStore<PK, K, V>;
}

impl<PK, K, V> NestedMapStoreExt<PK, K, V> for NestedMap<PK, K, V>
where
    PK: Hash + Eq + CellValue,
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    fn into_leptos_store(&self) -> NestedMapStore<PK, K, V> {
        NestedMapStore::new(self)
    }
}

#[cfg(test)]
mod tests {
    use hyphae::CellMap;
    use leptos::prelude::{GetUntracked, Owner};

    use super::{CellMapStoreExt, NestedMapStoreExt};

    #[test]
    fn cellmap_store_applies_diffs_per_key() {
        let owner = Owner::new();
        owner.with(|| {
            let map: CellMap<u32, String> = CellMap::new();
            map.insert(1, "a".into());

            // Initial snapshot is applied synchronously on subscribe.
            let store = map.into_leptos_store();
            assert!(store.contains_key(&1));
            assert_eq!(store.get(&1), Some("a".to_string()));
            assert_eq!(store.keys().get_untracked(), vec![1]);

            // Per-key signal updates in place on an Update diff.
            let sig = store.value(&1);
            assert_eq!(sig.get_untracked(), Some("a".to_string()));
            map.insert(1, "z".into());
            assert_eq!(sig.get_untracked(), Some("z".to_string()));

            // Insert extends the key list.
            map.insert(2, "b".into());
            assert!(store.contains_key(&2));
            let mut keys = store.keys().get_untracked();
            keys.sort_unstable();
            assert_eq!(keys, vec![1, 2]);

            // Remove drops the key from the live list and clears its signal,
            // but keeps the (now `None`) signal for subscribe-before-data.
            map.remove(&1);
            assert!(!store.contains_key(&1));
            assert_eq!(store.keys().get_untracked(), vec![2]);
            assert_eq!(sig.get_untracked(), None);
        });
    }

    #[test]
    fn value_subscribes_before_data() {
        let owner = Owner::new();
        owner.with(|| {
            let map: CellMap<u32, String> = CellMap::new();
            let store = map.into_leptos_store();

            // Bind to a key that doesn't exist yet — `None` for now.
            let sig = store.value(&7);
            assert_eq!(sig.get_untracked(), None);
            assert!(!store.contains_key(&7));

            // When the key arrives, the same signal lights up.
            map.insert(7, "hello".into());
            assert_eq!(sig.get_untracked(), Some("hello".to_string()));
            assert!(store.contains_key(&7));
        });
    }

    #[test]
    fn nested_map_store_groups_by_parent() {
        let owner = Owner::new();
        owner.with(|| {
            // value = "parent:child"; group by the parent prefix.
            let orders: CellMap<u32, String> = CellMap::new();
            orders.insert(1, "cust_a".into());
            orders.insert(2, "cust_a".into());
            orders.insert(3, "cust_b".into());

            let nested = orders.nest(|v: &String| v.clone());
            let store = nested.into_leptos_store();

            let mut parents = store.parents().get_untracked();
            parents.sort();
            assert_eq!(parents, vec!["cust_a".to_string(), "cust_b".to_string()]);

            let group_a = store.group(&"cust_a".to_string()).expect("group a");
            assert_eq!(group_a.len(), 2);

            let group_b = store.group(&"cust_b".to_string()).expect("group b");
            assert_eq!(group_b.len(), 1);
        });
    }
}
