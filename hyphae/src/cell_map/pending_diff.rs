use super::MapDiff;

/// Most narrow maps publish once in a transaction. Inline that first payload
/// instead of allocating a Vec that would then be retained as the latest diff.
#[derive(Default)]
pub(super) enum PendingDiff<K, V> {
    #[default]
    Empty,
    One(MapDiff<K, V>),
    Many(Vec<MapDiff<K, V>>),
}

impl<K, V> PendingDiff<K, V> {
    /// Whether this starts a new scheduled publication.
    pub(super) fn push(&mut self, diff: MapDiff<K, V>) -> bool {
        match std::mem::replace(self, Self::Empty) {
            Self::Empty => {
                *self = Self::One(diff);
                true
            }
            Self::One(first) => {
                *self = Self::Many(vec![first, diff]);
                false
            }
            Self::Many(mut changes) => {
                changes.push(diff);
                *self = Self::Many(changes);
                false
            }
        }
    }

    pub(super) fn take(&mut self) -> Option<MapDiff<K, V>> {
        match std::mem::replace(self, Self::Empty) {
            Self::Empty => None,
            Self::One(diff) => Some(diff),
            Self::Many(changes) => Some(MapDiff::Batch { changes }),
        }
    }
}
