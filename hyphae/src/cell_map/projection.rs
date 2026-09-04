use std::{collections::HashMap, hash::Hash, sync::Arc};

use crate::traits::CellValue;

use super::MapDiff;

#[derive(Clone)]
pub(super) struct OrderedProjection<K, T>
where
    K: Hash + Eq + CellValue,
{
    items: Vec<T>,
    keys: Vec<K>,
    index_by_key: HashMap<K, usize>,
}

impl<K, T> OrderedProjection<K, T>
where
    K: Hash + Eq + CellValue,
{
    pub(super) fn from_pairs(pairs: Vec<(K, T)>) -> Self {
        let mut items = Vec::with_capacity(pairs.len());
        let mut keys = Vec::with_capacity(pairs.len());
        let mut index_by_key = HashMap::with_capacity(pairs.len());

        for (idx, (key, item)) in pairs.into_iter().enumerate() {
            index_by_key.insert(key.clone(), idx);
            keys.push(key);
            items.push(item);
        }

        Self {
            items,
            keys,
            index_by_key,
        }
    }

    pub(super) fn replace(&mut self, pairs: Vec<(K, T)>) {
        *self = Self::from_pairs(pairs);
    }

    pub(super) fn items(&self) -> &[T] {
        &self.items
    }

    pub(super) fn keys(&self) -> &[K] {
        &self.keys
    }

    pub(super) fn contains_key(&self, key: &K) -> bool {
        self.index_by_key.contains_key(key)
    }

    pub(super) fn item_mut(&mut self, key: &K) -> Option<&mut T> {
        self.index_by_key
            .get(key)
            .and_then(|index| self.items.get_mut(*index))
    }

    pub(super) fn upsert(&mut self, key: K, item: T) -> bool {
        if let Some(existing) = self.item_mut(&key) {
            *existing = item;
            false
        } else {
            let index = self.items.len();
            self.index_by_key.insert(key.clone(), index);
            self.keys.push(key);
            self.items.push(item);
            true
        }
    }

    pub(super) fn swap_remove(&mut self, key: &K) -> Option<T> {
        let index = self.index_by_key.remove(key)?;
        self.keys.swap_remove(index);
        let item = self.items.swap_remove(index);
        if let Some(swapped_key) = self.keys.get(index) {
            self.index_by_key.insert(swapped_key.clone(), index);
        }
        Some(item)
    }
}

#[derive(Clone)]
pub(super) struct EntryProjection<K, V>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    store: OrderedProjection<K, (K, V)>,
}

impl<K, V> EntryProjection<K, V>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    pub(super) fn from_entries(entries: Vec<(K, V)>) -> Self {
        let pairs = entries
            .into_iter()
            .map(|(key, value)| (key.clone(), (key, value)))
            .collect();

        Self {
            store: OrderedProjection::from_pairs(pairs),
        }
    }

    pub(super) fn entries(&self) -> Vec<(K, V)> {
        self.store.items().to_vec()
    }

    pub(super) fn apply_diff(&mut self, diff: &MapDiff<K, V>) {
        match diff {
            MapDiff::Initial { entries } => *self = Self::from_entries(entries.clone()),
            MapDiff::Insert { key, value } => {
                if let Some((_, existing)) = self.store.item_mut(key) {
                    *existing = value.clone();
                } else {
                    self.store.upsert(key.clone(), (key.clone(), value.clone()));
                }
            }
            MapDiff::Remove { key, .. } => {
                self.store.swap_remove(key);
            }
            MapDiff::Update { key, new_value, .. } => {
                if let Some((_, existing)) = self.store.item_mut(key) {
                    *existing = new_value.clone();
                }
            }
            MapDiff::Batch { changes } => {
                for change in changes {
                    self.apply_diff(change);
                }
            }
        }
    }
}

#[derive(Clone)]
pub(super) struct KeyProjection<K>
where
    K: Hash + Eq + CellValue,
{
    store: OrderedProjection<K, ()>,
}

impl<K> KeyProjection<K>
where
    K: Hash + Eq + CellValue,
{
    pub(super) fn from_keys(keys: Vec<K>) -> Self {
        let pairs = keys.into_iter().map(|key| (key, ())).collect();

        Self {
            store: OrderedProjection::from_pairs(pairs),
        }
    }

    pub(super) fn keys(&self) -> Vec<K> {
        self.store.keys().to_vec()
    }

    pub(super) fn apply_diff<V: CellValue>(&mut self, diff: &MapDiff<K, V>) -> bool {
        match diff {
            MapDiff::Initial { entries } => {
                let keys: Vec<K> = entries.iter().map(|(key, _)| key.clone()).collect();
                if self.store.keys() == keys.as_slice() {
                    false
                } else {
                    self.store
                        .replace(keys.into_iter().map(|key| (key, ())).collect());
                    true
                }
            }
            MapDiff::Insert { key, .. } => {
                if self.store.contains_key(key) {
                    false
                } else {
                    self.store.upsert(key.clone(), ());
                    true
                }
            }
            MapDiff::Remove { key, .. } => self.store.swap_remove(key).is_some(),
            MapDiff::Update { .. } => false,
            MapDiff::Batch { changes } => {
                let mut changed = false;
                for change in changes {
                    changed |= self.apply_diff(change);
                }
                changed
            }
        }
    }
}

#[derive(Clone)]
pub(super) struct ValueProjection<K, V>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    store: OrderedProjection<K, V>,
}

impl<K, V> ValueProjection<K, V>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    pub(super) fn from_entries(entries: Vec<(K, V)>) -> Self {
        Self {
            store: OrderedProjection::from_pairs(entries),
        }
    }

    pub(super) fn items(&self) -> Vec<V> {
        self.store.items().to_vec()
    }

    pub(super) fn apply_diff(&mut self, diff: &MapDiff<K, V>) {
        match diff {
            MapDiff::Initial { entries } => *self = Self::from_entries(entries.clone()),
            MapDiff::Insert { key, value } => {
                if let Some(existing) = self.store.item_mut(key) {
                    *existing = value.clone();
                } else {
                    self.store.upsert(key.clone(), value.clone());
                }
            }
            MapDiff::Remove { key, .. } => {
                self.store.swap_remove(key);
            }
            MapDiff::Update { key, new_value, .. } => {
                if let Some(existing) = self.store.item_mut(key) {
                    *existing = new_value.clone();
                }
            }
            MapDiff::Batch { changes } => {
                for change in changes {
                    self.apply_diff(change);
                }
            }
        }
    }
}

pub(super) struct ProjectionOwner<P> {
    pub(super) state: Arc<std::sync::Mutex<P>>,
}

impl<P> ProjectionOwner<P> {
    pub(super) fn new(projection: P) -> Self {
        Self {
            state: Arc::new(std::sync::Mutex::new(projection)),
        }
    }

    pub(super) fn with<R>(&self, apply: impl FnOnce(&mut P) -> R) -> R {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        apply(&mut state)
    }
}

impl<P> Clone for ProjectionOwner<P> {
    fn clone(&self) -> Self {
        Self {
            state: Arc::clone(&self.state),
        }
    }
}
