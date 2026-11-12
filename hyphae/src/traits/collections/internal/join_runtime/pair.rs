//! Pair joins maintain only pairs incident to changed source rows. Aggregate
//! joins still use the general runtime because their values can depend on an
//! entire matching bucket.
use std::{
    collections::hash_map::Entry,
    hash::Hash,
    sync::{Arc, Mutex},
};

use rustc_hash::FxHashMap;

use super::super::ordered_set::OrderedSet;
use crate::{
    cell_map::MapDiff,
    map_query::{BoxedMapDiffSink, MapQuery, compiler::CompileContext},
    subscription::SubscriptionGuard,
    traits::{CellValue, RightJoinKey},
};

struct Row<V, J> {
    value: V,
    join: Option<J>,
}
struct Side<K, V, J> {
    rows: FxHashMap<K, Row<V, J>>,
    buckets: FxHashMap<J, OrderedSet<K>>,
}
impl<K, V, J> Default for Side<K, V, J> {
    fn default() -> Self {
        Self {
            rows: FxHashMap::default(),
            buckets: FxHashMap::default(),
        }
    }
}
struct Touched<K, J> {
    order: OrderedSet<K>,
    previous: FxHashMap<K, Option<J>>,
    work: Vec<K>,
    initial: bool,
}
impl<K, J> Default for Touched<K, J> {
    fn default() -> Self {
        Self {
            order: OrderedSet::default(),
            previous: FxHashMap::default(),
            work: Vec::new(),
            initial: false,
        }
    }
}
impl<K, V, J> Side<K, V, J>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
    J: Hash + Eq + CellValue,
{
    fn remember(&self, key: &K, touched: &mut Touched<K, J>) {
        if touched.order.insert(key.clone()) {
            touched.previous.insert(
                key.clone(),
                self.rows.get(key).and_then(|row| row.join.clone()),
            );
        }
    }
    fn remove_index(&mut self, join: &J, key: &K) {
        if let Some(bucket) = self.buckets.get_mut(join) {
            bucket.remove(key);
            if bucket.is_empty() {
                self.buckets.remove(join);
            }
        }
    }
    fn upsert(&mut self, key: &K, value: &V, join: Option<J>, touched: &mut Touched<K, J>) {
        self.remember(key, touched);
        let previous = self.rows.get(key).and_then(|row| row.join.clone());
        // A value update in the same bucket must not scan or reorder siblings.
        if previous != join {
            if let Some(previous) = previous {
                self.remove_index(&previous, key);
            }
            if let Some(join) = &join {
                self.buckets
                    .entry(join.clone())
                    .or_default()
                    .insert(key.clone());
            }
        }
        self.rows.insert(
            key.clone(),
            Row {
                value: value.clone(),
                join,
            },
        );
    }
    fn apply(
        &mut self,
        diff: &MapDiff<K, V>,
        key: &impl Fn(&K, &V) -> Option<J>,
        touched: &mut Touched<K, J>,
    ) {
        match diff {
            MapDiff::Initial { entries } => {
                touched.initial = true;
                for row in self.rows.keys() {
                    self.remember(row, touched);
                }
                self.rows.clear();
                self.buckets.clear();
                for (row, value) in entries {
                    self.upsert(row, value, key(row, value), touched);
                }
            }
            MapDiff::Insert { key: row, value } => {
                self.upsert(row, value, key(row, value), touched);
            }
            MapDiff::Update {
                key: row,
                new_value,
                ..
            } => self.upsert(row, new_value, key(row, new_value), touched),
            MapDiff::Remove { key: row, .. } => {
                self.remember(row, touched);
                if let Some(old) = self.rows.remove(row).and_then(|row| row.join) {
                    self.remove_index(&old, row);
                }
            }
            MapDiff::Batch { changes } => {
                for change in changes {
                    self.apply(change, key, touched);
                }
            }
        }
    }
}

/// The opposite side is unchanged during one serialized root callback. Old and
/// new buckets are disjoint unless their key is equal, so every affected pair
/// is visited once without inspecting unchanged same-side bucket siblings.
fn affected<K, V, OK, OV, J>(
    changed: &Side<K, V, J>,
    other: &Side<OK, OV, J>,
    touched: &mut Touched<K, J>,
    mut visit: impl FnMut(&K, &OK, Option<(&V, &OV)>),
) where
    K: Hash + Eq + CellValue,
    V: CellValue,
    OK: Hash + Eq + CellValue,
    OV: CellValue,
    J: Hash + Eq + CellValue,
{
    touched.order.move_into(&mut touched.work);
    for key in &touched.work {
        let previous = touched.previous.remove(key).flatten();
        let next = changed.rows.get(key);
        let next_join = next.and_then(|row| row.join.as_ref());
        let mut bucket = |join: &J| {
            if let Some(keys) = other.buckets.get(join) {
                for other_key in keys.iter() {
                    let value =
                        next.filter(|row| row.join.as_ref() == Some(join))
                            .and_then(|row| {
                                other
                                    .rows
                                    .get(other_key)
                                    .map(|other| (&row.value, &other.value))
                            });
                    #[cfg(feature = "profiling")]
                    tracing::trace!(target: "hyphae::work", work = "pair_join_visit", label = std::any::type_name::<(V, OV)>());
                    visit(key, other_key, value);
                }
            }
        };
        if let Some(previous) = &previous {
            bucket(previous);
        }
        if next_join != previous.as_ref()
            && let Some(next) = next_join
        {
            bucket(next);
        }
    }
    // Initial snapshots can touch every source row even when no pair matches.
    // Keep small buffers warm, but do not retain a full snapshot's scratch for
    // the point updates that follow it. The authoritative rows/indexes survive.
    if touched.initial && touched.work.len() > 256 {
        *touched = Touched::default();
    } else {
        touched.work.clear();
        touched.initial = false;
    }
}

// Retain the last published value, not just source snapshots. PartialEq may
// ignore fields, so an equal source update can leave a different allocation in
// the published map; a later diff's old_value must still be that published one.
fn publish<K, V>(
    cache: &mut FxHashMap<K, V>,
    key: K,
    value: Option<V>,
    changes: &mut Vec<MapDiff<K, V>>,
) where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    if let Some(value) = value {
        match cache.entry(key.clone()) {
            Entry::Occupied(mut entry) => {
                if entry.get() != &value {
                    let old_value = entry.insert(value.clone());
                    changes.push(MapDiff::Update {
                        key,
                        old_value,
                        new_value: value,
                    });
                }
            }
            Entry::Vacant(entry) => {
                entry.insert(value.clone());
                changes.push(MapDiff::Insert { key, value });
            }
        }
    } else if let Some(old_value) = cache.remove(&key) {
        changes.push(MapDiff::Remove { key, old_value });
    }
}
struct State<LK, LV, RK, RV, J> {
    left: Side<LK, LV, J>,
    right: Side<RK, RV, J>,
    output: FxHashMap<(LK, RK), (LV, RV)>,
    left_touched: Touched<LK, J>,
    right_touched: Touched<RK, J>,
}
impl<LK, LV, RK, RV, J> Default for State<LK, LV, RK, RV, J> {
    fn default() -> Self {
        Self {
            left: Side::default(),
            right: Side::default(),
            output: FxHashMap::default(),
            left_touched: Touched::default(),
            right_touched: Touched::default(),
        }
    }
}

pub fn install_pair_join_runtime_via_query<LK, LV, RK, RV, J, L, R, FL, FR>(
    cx: &mut CompileContext,
    left: L,
    right: R,
    left_key: FL,
    right_key: FR,
    sink: BoxedMapDiffSink<(LK, RK), (LV, RV)>,
) -> Vec<SubscriptionGuard>
where
    LK: Hash + Eq + CellValue,
    LV: CellValue,
    RK: Hash + Eq + CellValue,
    RV: CellValue,
    J: Hash + Eq + CellValue,
    L: MapQuery<Key = LK, Value = LV>,
    R: MapQuery<Key = RK, Value = RV>,
    FL: Fn(&LK, &LV) -> J + Send + Sync + 'static,
    FR: RightJoinKey<RK, RV, J>,
{
    let state = Arc::new(Mutex::new(State::<LK, LV, RK, RV, J>::default()));
    let left_sink = {
        let state = state.clone();
        let sink = sink.clone();
        move |diff: &MapDiff<LK, LV>| {
            let mut state = state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut touched = std::mem::take(&mut state.left_touched);
            state
                .left
                .apply(diff, &|key, value| Some(left_key(key, value)), &mut touched);
            let State {
                left,
                right,
                output,
                ..
            } = &mut *state;
            let mut changes = Vec::new();
            affected(left, right, &mut touched, |lk, rk, values| {
                publish(
                    output,
                    (lk.clone(), rk.clone()),
                    values.map(|(lv, rv)| (lv.clone(), rv.clone())),
                    &mut changes,
                );
            });
            state.left_touched = touched;
            // Keep publication in mutation order when root callbacks race.
            super::emit_changes(&sink, changes);
            drop(state);
        }
    };
    let right_sink = move |diff: &MapDiff<RK, RV>| {
        let mut state = state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut touched = std::mem::take(&mut state.right_touched);
        state.right.apply(
            diff,
            &|key, value| right_key.right_join_key(key, value),
            &mut touched,
        );
        let State {
            left,
            right,
            output,
            ..
        } = &mut *state;
        let mut changes = Vec::new();
        affected(right, left, &mut touched, |rk, lk, values| {
            publish(
                output,
                (lk.clone(), rk.clone()),
                values.map(|(rv, lv)| (lv.clone(), rv.clone())),
                &mut changes,
            );
        });
        state.right_touched = touched;
        super::emit_changes(&sink, changes);
        drop(state);
    };
    let mut guards = crate::map_query::compile_runtime_into(left, cx, Arc::new(left_sink));
    guards.extend(crate::map_query::compile_runtime_into(
        right,
        cx,
        Arc::new(right_sink),
    ));
    guards
}

#[cfg(test)]
mod tests {
    use super::{Side, Touched, affected};
    use crate::MapDiff;

    #[test]
    fn initial_join_does_not_retain_large_touched_scratch() {
        let mut side = Side::<usize, usize, usize>::default();
        let other = Side::<usize, usize, usize>::default();
        let mut touched = Touched::default();
        side.apply(
            &MapDiff::Initial {
                entries: (0..16_384).map(|key| (key, key)).collect(),
            },
            &|_, value| Some(*value),
            &mut touched,
        );
        affected(&side, &other, &mut touched, |_, _, _| {});
        assert_eq!(side.rows.len(), 16_384);
        assert_eq!(side.buckets.len(), 16_384);
        eprintln!(
            "post-initial scratch capacities: previous={}, work={}",
            touched.previous.capacity(),
            touched.work.capacity()
        );
        assert!(touched.previous.capacity() <= 256);
        assert!(touched.work.capacity() <= 256);
    }
}
