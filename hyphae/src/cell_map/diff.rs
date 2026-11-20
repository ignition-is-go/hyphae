/// Diff notification for map changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MapDiff<K, V> {
    /// Initial snapshot of all entries when subscribing.
    Initial { entries: Vec<(K, V)> },
    /// A new key was inserted.
    Insert { key: K, value: V },
    /// A key was removed.
    Remove { key: K, old_value: V },
    /// An existing key's value was updated.
    Update { key: K, old_value: V, new_value: V },
    /// Multiple diffs emitted as a single notification.
    Batch { changes: Vec<Self> },
}

impl<K, V> MapDiff<K, V> {
    pub(crate) const fn atomic_key(&self) -> Option<&K> {
        match self {
            Self::Insert { key, .. } | Self::Remove { key, .. } | Self::Update { key, .. } => {
                Some(key)
            }
            Self::Initial { .. } | Self::Batch { .. } => None,
        }
    }

    pub(crate) fn work_items(&self) -> usize {
        match self {
            Self::Initial { entries } => entries.len(),
            Self::Batch { changes } => changes
                .iter()
                .fold(0, |total, change| total.saturating_add(change.work_items())),
            Self::Insert { .. } | Self::Remove { .. } | Self::Update { .. } => 1,
        }
    }

    /// Fold `next` into `held` so one notification carries both, in order.
    /// Returns `next` when either is an `Initial` snapshot, which is never
    /// placed inside a batch.
    pub(crate) fn compose(
        held: &mut std::sync::Arc<Self>,
        next: std::sync::Arc<Self>,
    ) -> Option<std::sync::Arc<Self>>
    where
        K: Clone,
        V: Clone,
    {
        if matches!(**held, Self::Initial { .. }) || matches!(*next, Self::Initial { .. }) {
            return Some(next);
        }
        let next = std::sync::Arc::unwrap_or_clone(next);
        let held = std::sync::Arc::make_mut(held);
        if let Self::Batch { changes } = held {
            next.flatten_into(changes);
        } else {
            let first = std::mem::replace(
                held,
                Self::Batch {
                    changes: Vec::new(),
                },
            );
            if let Self::Batch { changes } = held {
                changes.push(first);
                next.flatten_into(changes);
            }
        }
        None
    }

    /// Undo this diff on `held`, which holds the entries after it.
    pub(crate) fn undo(&self, held: &mut std::collections::HashMap<K, V>)
    where
        K: std::hash::Hash + Eq + Clone,
        V: Clone,
    {
        match self {
            Self::Insert { key, .. } => {
                held.remove(key);
            }
            Self::Remove { key, old_value } | Self::Update { key, old_value, .. } => {
                held.insert(key.clone(), old_value.clone());
            }
            Self::Batch { changes } => changes.iter().rev().for_each(|change| change.undo(held)),
            Self::Initial { .. } => {}
        }
    }

    pub(crate) fn visit_leaves<'a>(&'a self, visit: &mut impl FnMut(&'a Self)) {
        match self {
            Self::Batch { changes } => {
                for change in changes {
                    change.visit_leaves(visit);
                }
            }
            change => visit(change),
        }
    }

    pub(crate) fn flatten_into(self, output: &mut Vec<Self>) {
        match self {
            Self::Batch { changes } => {
                for change in changes {
                    change.flatten_into(output);
                }
            }
            change => output.push(change),
        }
    }
}
