use std::{collections::HashMap, hash::Hash, sync::Arc};

use crate::traits::CellValue;

use super::MapDiff;

#[derive(Clone)]
pub(super) struct EntryProjection<K, V>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    pub(super) entries: Vec<(K, V)>,
    index_by_key: HashMap<K, usize>,
}

impl<K, V> EntryProjection<K, V>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    pub(super) fn from_entries(entries: Vec<(K, V)>) -> Self {
        let index_by_key = entries
            .iter()
            .enumerate()
            .map(|(idx, (key, _))| (key.clone(), idx))
            .collect();

        Self {
            entries,
            index_by_key,
        }
    }

    pub(super) fn apply_diff(&mut self, diff: &MapDiff<K, V>) {
        match diff {
            MapDiff::Initial { entries } => {
                *self = Self::from_entries(entries.clone());
            }
            MapDiff::Insert { key, value } => {
                if let Some(entry) = self
                    .index_by_key
                    .get(key)
                    .and_then(|index| self.entries.get_mut(*index))
                {
                    entry.1 = value.clone();
                    return;
                }
                let idx = self.entries.len();
                self.entries.push((key.clone(), value.clone()));
                self.index_by_key.insert(key.clone(), idx);
            }
            MapDiff::Remove { key, .. } => self.remove_key(key),
            MapDiff::Update { key, new_value, .. } => {
                if let Some(entry) = self
                    .index_by_key
                    .get(key)
                    .and_then(|index| self.entries.get_mut(*index))
                {
                    entry.1 = new_value.clone();
                }
            }
            MapDiff::Batch { changes } => {
                for change in changes {
                    self.apply_diff(change);
                }
            }
        }
    }

    fn remove_key(&mut self, key: &K) {
        let Some(idx) = self.index_by_key.remove(key) else {
            return;
        };
        self.entries.swap_remove(idx);
        if idx < self.entries.len()
            && let Some((swapped_key, _)) = self.entries.get(idx)
        {
            self.index_by_key.insert(swapped_key.clone(), idx);
        }
    }
}

#[derive(Clone)]
pub(super) struct KeyProjection<K>
where
    K: Hash + Eq + CellValue,
{
    pub(super) keys: Vec<K>,
    index_by_key: HashMap<K, usize>,
}

impl<K> KeyProjection<K>
where
    K: Hash + Eq + CellValue,
{
    pub(super) fn from_keys(keys: Vec<K>) -> Self {
        let index_by_key = keys
            .iter()
            .enumerate()
            .map(|(idx, key)| (key.clone(), idx))
            .collect();

        Self { keys, index_by_key }
    }

    pub(super) fn apply_diff<V: CellValue>(&mut self, diff: &MapDiff<K, V>) -> bool {
        match diff {
            MapDiff::Initial { entries } => {
                let keys = entries.iter().map(|(key, _)| key.clone()).collect();
                if self.keys == keys {
                    false
                } else {
                    *self = Self::from_keys(keys);
                    true
                }
            }
            MapDiff::Insert { key, .. } => {
                if self.index_by_key.contains_key(key) {
                    return false;
                }
                let idx = self.keys.len();
                self.keys.push(key.clone());
                self.index_by_key.insert(key.clone(), idx);
                true
            }
            MapDiff::Remove { key, .. } => self.remove_key(key),
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

    fn remove_key(&mut self, key: &K) -> bool {
        let Some(idx) = self.index_by_key.remove(key) else {
            return false;
        };
        self.keys.swap_remove(idx);
        if idx < self.keys.len()
            && let Some(swapped_key) = self.keys.get(idx)
        {
            self.index_by_key.insert(swapped_key.clone(), idx);
        }
        true
    }
}

#[derive(Clone)]
pub(super) struct ValueProjection<K, V>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    pub(super) items: Vec<V>,
    index_by_key: HashMap<K, usize>,
    keys_by_index: Vec<K>,
}

impl<K, V> ValueProjection<K, V>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    pub(super) fn from_entries(entries: Vec<(K, V)>) -> Self {
        let mut items = Vec::with_capacity(entries.len());
        let mut keys_by_index = Vec::with_capacity(entries.len());
        let mut index_by_key = HashMap::with_capacity(entries.len());

        for (idx, (key, value)) in entries.into_iter().enumerate() {
            index_by_key.insert(key.clone(), idx);
            keys_by_index.push(key);
            items.push(value);
        }

        Self {
            items,
            index_by_key,
            keys_by_index,
        }
    }

    pub(super) fn apply_diff(&mut self, diff: &MapDiff<K, V>) {
        match diff {
            MapDiff::Initial { entries } => {
                *self = Self::from_entries(entries.clone());
            }
            MapDiff::Insert { key, value } => {
                if let Some(item) = self
                    .index_by_key
                    .get(key)
                    .and_then(|index| self.items.get_mut(*index))
                {
                    *item = value.clone();
                    return;
                }
                let idx = self.items.len();
                self.index_by_key.insert(key.clone(), idx);
                self.keys_by_index.push(key.clone());
                self.items.push(value.clone());
            }
            MapDiff::Remove { key, .. } => self.remove_key(key),
            MapDiff::Update { key, new_value, .. } => {
                if let Some(item) = self
                    .index_by_key
                    .get(key)
                    .and_then(|index| self.items.get_mut(*index))
                {
                    *item = new_value.clone();
                }
            }
            MapDiff::Batch { changes } => {
                for change in changes {
                    self.apply_diff(change);
                }
            }
        }
    }

    fn remove_key(&mut self, key: &K) {
        let Some(idx) = self.index_by_key.remove(key) else {
            return;
        };
        self.items.swap_remove(idx);
        self.keys_by_index.swap_remove(idx);
        if idx < self.keys_by_index.len()
            && let Some(swapped_key) = self.keys_by_index.get(idx)
        {
            self.index_by_key.insert(swapped_key.clone(), idx);
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
