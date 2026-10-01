use std::collections::BTreeMap;

const SMALL_LIMIT: usize = 32;

pub(super) struct Frontier<K, V> {
    small: Vec<(K, V)>,
    #[allow(
        clippy::box_collection,
        reason = "keep uncommon tree state out of the small frontier's inline storage"
    )]
    large: Option<Box<BTreeMap<K, V>>>,
}

impl<K: Ord, V> Frontier<K, V> {
    pub(super) const fn new() -> Self {
        Self {
            small: Vec::new(),
            large: None,
        }
    }

    pub(super) fn insert(&mut self, key: K, value: V) -> Option<V> {
        if let Some(large) = self.large.as_mut() {
            return large.insert(key, value);
        }

        match self
            .small
            .binary_search_by(|(candidate, _)| candidate.cmp(&key))
        {
            Ok(position) => self
                .small
                .get_mut(position)
                .map(|(_, current)| std::mem::replace(current, value)),
            Err(position) => {
                self.small.insert(position, (key, value));
                if self.small.len() > SMALL_LIMIT {
                    let mut large = Box::new(BTreeMap::new());
                    large.extend(self.small.drain(..));
                    self.large = Some(large);
                }
                None
            }
        }
    }

    #[cfg(test)]
    pub(super) fn get(&self, key: &K) -> Option<&V> {
        if let Some(large) = self.large.as_ref() {
            return large.get(key);
        }
        let position = self
            .small
            .binary_search_by(|(candidate, _)| candidate.cmp(key))
            .ok()?;
        self.small.get(position).map(|(_, value)| value)
    }

    #[cfg(test)]
    pub(super) const fn is_large(&self) -> bool {
        self.large.is_some()
    }

    #[cfg(test)]
    pub(super) fn for_each(&self, mut visit: impl FnMut(&K, &V)) {
        if let Some(large) = self.large.as_ref() {
            for (key, value) in large.iter() {
                visit(key, value);
            }
        } else {
            for (key, value) in &self.small {
                visit(key, value);
            }
        }
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.large
            .as_ref()
            .map_or(self.small.len(), |large| large.len())
    }

    pub(super) fn get_mut(&mut self, key: &K) -> Option<&mut V> {
        if let Some(large) = self.large.as_mut() {
            return large.get_mut(key);
        }
        let position = self
            .small
            .binary_search_by(|(candidate, _)| candidate.cmp(key))
            .ok()?;
        self.small.get_mut(position).map(|(_, value)| value)
    }

    pub(super) fn remove(&mut self, key: &K) -> Option<V> {
        if self.large.is_some() {
            let value = self.large.as_mut().and_then(|large| large.remove(key));
            self.release_empty_tree();
            return value;
        }
        let position = self
            .small
            .binary_search_by(|(candidate, _)| candidate.cmp(key))
            .ok()?;
        Some(self.small.remove(position).1)
    }

    pub(super) fn first_key_value(&self) -> Option<(&K, &V)> {
        if let Some(large) = self.large.as_ref() {
            return large.first_key_value();
        }
        self.small.first().map(|(key, value)| (key, value))
    }

    pub(super) fn pop_first(&mut self) -> Option<(K, V)> {
        if self.large.is_some() {
            let value = self.large.as_mut().and_then(|large| large.pop_first());
            self.release_empty_tree();
            return value;
        }
        (!self.small.is_empty()).then(|| self.small.remove(0))
    }

    pub(super) fn is_empty(&self) -> bool {
        self.large
            .as_ref()
            .map_or_else(|| self.small.is_empty(), |large| large.is_empty())
    }

    fn release_empty_tree(&mut self) {
        if self.large.as_ref().is_some_and(|large| large.is_empty()) {
            self.large = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn promotes_drains_and_reuses_small_storage() {
        let mut frontier = Frontier::new();
        for key in (0..64).rev() {
            assert!(frontier.insert(key, key).is_none());
        }
        assert!(frontier.large.is_some());
        let retained_capacity = frontier.small.capacity();

        for expected in 0..64 {
            assert_eq!(frontier.pop_first(), Some((expected, expected)));
        }
        assert!(frontier.large.is_none());
        assert!(frontier.small.capacity() >= retained_capacity);
        assert!(frontier.insert(2, 2).is_none());
        assert!(frontier.insert(1, 1).is_none());
        assert_eq!(frontier.pop_first(), Some((1, 1)));
        assert_eq!(frontier.pop_first(), Some((2, 2)));
    }
}
