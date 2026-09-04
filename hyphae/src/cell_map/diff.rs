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
