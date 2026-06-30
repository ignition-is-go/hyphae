//! Bridge hyphae `CellMap`/`NestedMap` into fine-grained Leptos reactive state.
//!
//! Rather than one coarse `RwSignal<HashMap<..>>` (where any change wakes every
//! reader), each key owns its own `RwSignal<V>`. A `MapDiff::Update` to one key
//! re-runs only the views reading that key — the same fine-grained behaviour
//! Leptos `reactive_stores` give a static struct, applied to a dynamic,
//! diff-driven collection.

use std::{collections::HashMap, hash::Hash, ops::Deref};

use hyphae::{CellMap, CellValue, MapDiff, NestedMap, SubscriptionGuard};
use leptos::prelude::{
    Dispose, GetUntracked, RwSignal, Set, Signal, StoredValue, Update, UpdateValue, WithValue,
    on_cleanup,
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
    /// Per-key value signals. Held in a `StoredValue` so the map is owned by the
    /// reactive arena and `Copy`/`Send`/`Sync` for use in subscription closures.
    values: StoredValue<HashMap<K, RwSignal<V>>>,
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

    /// The reactive value signal for `key`, if present. Reading it tracks only
    /// this key, so a change to another key won't re-run this view.
    pub fn value(&self, key: &K) -> Option<RwSignal<V>> {
        self.values.with_value(|m| m.get(key).copied())
    }

    /// Non-reactive snapshot of a key's current value.
    pub fn get(&self, key: &K) -> Option<V> {
        self.values
            .with_value(|m| m.get(key).map(|s| s.get_untracked()))
    }

    /// Whether `key` currently exists.
    pub fn contains_key(&self, key: &K) -> bool {
        self.values.with_value(|m| m.contains_key(key))
    }

    /// Number of entries (non-reactive).
    pub fn len(&self) -> usize {
        self.values.with_value(HashMap::len)
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
                self.values.update_value(|m| {
                    for (_, sig) in m.drain() {
                        sig.dispose();
                    }
                    for (k, v) in entries {
                        m.insert(k.clone(), RwSignal::new(v.clone()));
                    }
                });
                self.keys.set(entries.iter().map(|(k, _)| k.clone()).collect());
            }
            MapDiff::Insert { key, value } => self.upsert(key, value),
            MapDiff::Update {
                key, new_value, ..
            } => self.upsert(key, new_value),
            MapDiff::Remove { key, .. } => {
                let mut removed = false;
                self.values.update_value(|m| {
                    if let Some(sig) = m.remove(key) {
                        sig.dispose();
                        removed = true;
                    }
                });
                if removed {
                    self.keys.update(|ks| ks.retain(|k| k != key));
                }
            }
            MapDiff::Batch { changes } => {
                for change in changes {
                    self.apply(change);
                }
            }
        }
    }

    /// Set an existing key's value (fine-grained), or create its signal and
    /// extend the key list if it's new.
    fn upsert(&self, key: &K, value: &V) {
        let mut is_new = false;
        self.values.update_value(|m| match m.get(key) {
            Some(sig) => sig.set(value.clone()),
            None => {
                m.insert(key.clone(), RwSignal::new(value.clone()));
                is_new = true;
            }
        });
        if is_new {
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
            let sig = store.value(&1).expect("key 1 present");
            map.insert(1, "z".into());
            assert_eq!(sig.get_untracked(), "z".to_string());

            // Insert extends the key list.
            map.insert(2, "b".into());
            assert!(store.contains_key(&2));
            let mut keys = store.keys().get_untracked();
            keys.sort_unstable();
            assert_eq!(keys, vec![1, 2]);

            // Remove drops the key and disposes its signal.
            map.remove(&1);
            assert!(!store.contains_key(&1));
            assert_eq!(store.keys().get_untracked(), vec![2]);
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
