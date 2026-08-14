use std::{collections::HashMap, hash::Hash};

use gpui::{App, AppContext, Context, Entity, Task};
use hyphae::{CellMap, CellValue, MapDiff, SubscriptionGuard};

/// Fine-grained state for one map key.
pub struct MapEntry<V: CellValue> {
    value: Option<V>,
}

impl<V: CellValue> MapEntry<V> {
    /// Current value, or `None` after removal.
    #[must_use]
    pub const fn value(&self) -> Option<&V> {
        self.value.as_ref()
    }
}

/// GPUI representation of a Hyphae `CellMap`.
///
/// Observe this entity for membership changes. Observe an entity returned by
/// [`entry`](Self::entry) for updates to just one key.
pub struct CellMapEntity<K, V>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    keys: Vec<K>,
    entries: HashMap<K, Entity<MapEntry<V>>>,
    _subscription: SubscriptionGuard,
    _driver: Task<()>,
}

impl<K, V> CellMapEntity<K, V>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    /// Live keys. Their order follows Hyphae's diff order and is not sorted.
    #[must_use]
    pub fn keys(&self) -> &[K] {
        &self.keys
    }

    /// Fine-grained entity for a currently or previously present key.
    /// Removed entries are retained with a `None` value so existing observers
    /// see removal and the same entity can be reused on reinsertion.
    #[must_use]
    pub fn entry(&self, key: &K) -> Option<Entity<MapEntry<V>>> {
        self.entries.get(key).cloned()
    }

    fn apply(&mut self, diff: MapDiff<K, V>, cx: &mut Context<Self>) {
        match diff {
            MapDiff::Initial { entries } => {
                self.keys.clear();
                for (key, value) in entries {
                    self.upsert(key, value, cx);
                }
                cx.notify();
            }
            MapDiff::Insert { key, value } => {
                self.upsert(key, value, cx);
                cx.notify();
            }
            MapDiff::Update { key, new_value, .. } => {
                self.upsert(key, new_value, cx);
            }
            MapDiff::Remove { key, .. } => {
                self.keys.retain(|candidate| candidate != &key);
                if let Some(entry) = self.entries.get(&key) {
                    entry.update(cx, |entry, cx| {
                        entry.value = None;
                        cx.notify();
                    });
                }
                cx.notify();
            }
            MapDiff::Batch { changes } => {
                for change in changes {
                    self.apply(change, cx);
                }
            }
        }
    }

    fn upsert(&mut self, key: K, value: V, cx: &mut Context<Self>) {
        if let Some(entry) = self.entries.get(&key) {
            entry.update(cx, |entry, cx| {
                entry.value = Some(value);
                cx.notify();
            });
        } else {
            let entry = cx.new(|_| MapEntry { value: Some(value) });
            self.entries.insert(key.clone(), entry);
        }
        if !self.keys.contains(&key) {
            self.keys.push(key);
        }
    }
}

/// Convert a Hyphae cell-map into a fine-grained GPUI entity graph.
pub trait ToGpuiMapEntity<K, V>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    /// Subscribe to diffs and create a collection entity plus per-key entities.
    fn to_gpui_map_entity(&self, cx: &mut App) -> Entity<CellMapEntity<K, V>>;
}

impl<K, V, M> ToGpuiMapEntity<K, V> for CellMap<K, V, M>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
    M: Send + Sync + 'static,
{
    fn to_gpui_map_entity(&self, cx: &mut App) -> Entity<CellMapEntity<K, V>> {
        let (sender, receiver) = flume::unbounded();
        let subscription = self.subscribe_diffs(move |diff| {
            let _ = sender.send(diff.clone());
        });

        cx.new(move |cx| {
            let driver = cx.spawn(async move |entity, cx| {
                loop {
                    // See the cell bridge: background task completion is the
                    // cross-platform wake into GPUI's foreground scheduler.
                    let receive = receiver.clone();
                    let diff = cx
                        .background_executor()
                        .spawn(async move { receive.recv_async().await })
                        .await;
                    let Ok(diff) = diff else {
                        break;
                    };
                    if entity
                        .update(cx, |state: &mut CellMapEntity<K, V>, cx| {
                            state.apply(diff, cx);
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            });
            CellMapEntity {
                keys: Vec::new(),
                entries: HashMap::new(),
                _subscription: subscription,
                _driver: driver,
            }
        })
    }
}

#[cfg(test)]
#[allow(clippy::needless_pass_by_ref_mut)]
mod tests {
    use gpui::TestAppContext;
    use hyphae::CellMap;

    use super::ToGpuiMapEntity;

    #[gpui::test]
    fn updates_only_the_affected_entry_state(cx: &mut TestAppContext) {
        let map = CellMap::<u32, String>::new();
        map.insert(1, "one".into());
        map.insert(2, "two".into());
        let entity = cx.update(|cx| map.to_gpui_map_entity(cx));
        cx.run_until_parked();

        let first = entity.read_with(cx, |state, _| state.entry(&1));
        let second = entity.read_with(cx, |state, _| state.entry(&2));
        assert_eq!(
            first
                .as_ref()
                .and_then(|entry| entry.read_with(cx, |entry, _| entry.value().cloned())),
            Some("one".to_string())
        );
        assert_eq!(
            second
                .as_ref()
                .and_then(|entry| entry.read_with(cx, |entry, _| entry.value().cloned())),
            Some("two".to_string())
        );

        map.insert(1, "updated".into());
        cx.run_until_parked();
        assert_eq!(
            first.and_then(|entry| entry.read_with(cx, |entry, _| entry.value().cloned())),
            Some("updated".to_string())
        );
        assert_eq!(
            second.and_then(|entry| entry.read_with(cx, |entry, _| entry.value().cloned())),
            Some("two".to_string())
        );
    }

    #[gpui::test]
    fn removal_clears_the_existing_key_entity(cx: &mut TestAppContext) {
        let map = CellMap::<u32, String>::new();
        map.insert(1, "one".into());
        let entity = cx.update(|cx| map.to_gpui_map_entity(cx));
        cx.run_until_parked();
        let entry = entity.read_with(cx, |state, _| state.entry(&1));

        map.remove(&1);
        cx.run_until_parked();
        assert!(entity.read_with(cx, |state, _| state.keys().is_empty()));
        assert_eq!(
            entry.and_then(|entry| entry.read_with(cx, |entry, _| entry.value().cloned())),
            None
        );
    }
}
