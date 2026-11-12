//! Membership-only joins: changed left rows do not revisit bucket siblings.
use std::{
    collections::hash_map::Entry,
    hash::Hash,
    sync::{Arc, Mutex},
};

use rustc_hash::FxHashMap;

use super::super::ordered_set::OrderedSet;
use crate::{
    MapDiff,
    map_query::{BoxedMapDiffSink, MapQuery, compiler::CompileContext},
    subscription::SubscriptionGuard,
    traits::{CellValue, RightJoinKey},
};

struct MembershipTouched<J> {
    before: FxHashMap<J, bool>,
    order: OrderedSet<J>,
}
impl<J> Default for MembershipTouched<J> {
    fn default() -> Self {
        Self {
            before: FxHashMap::default(),
            order: OrderedSet::default(),
        }
    }
}

struct State<LK, LV, RK, J> {
    left: FxHashMap<LK, (LV, J)>,
    buckets: FxHashMap<J, OrderedSet<LK>>,
    right: FxHashMap<RK, Option<J>>,
    counts: FxHashMap<J, usize>,
    published: FxHashMap<LK, LV>,
}
impl<LK, LV, RK, J> Default for State<LK, LV, RK, J> {
    fn default() -> Self {
        Self {
            left: FxHashMap::default(),
            buckets: FxHashMap::default(),
            right: FxHashMap::default(),
            counts: FxHashMap::default(),
            published: FxHashMap::default(),
        }
    }
}
impl<LK, LV, RK, J> State<LK, LV, RK, J>
where
    LK: Hash + Eq + CellValue,
    LV: CellValue,
    RK: Hash + Eq + CellValue,
    J: Hash + Eq + CellValue,
{
    fn unindex(&mut self, key: &LK, join: &J) {
        if let Some(bucket) = self.buckets.get_mut(join) {
            bucket.remove(key);
            if bucket.is_empty() {
                self.buckets.remove(join);
            }
        }
    }
    fn set_left(&mut self, key: &LK, value: &LV, join: J, touched: &mut OrderedSet<LK>) {
        touched.insert(key.clone());
        if let Some((_, previous)) = self.left.get(key)
            && previous != &join
        {
            let previous = previous.clone();
            self.unindex(key, &previous);
        }
        self.buckets
            .entry(join.clone())
            .or_default()
            .insert(key.clone());
        self.left.insert(key.clone(), (value.clone(), join));
    }
    fn apply_left(
        &mut self,
        diff: &MapDiff<LK, LV>,
        extract: &impl Fn(&LK, &LV) -> J,
        touched: &mut OrderedSet<LK>,
    ) {
        match diff {
            MapDiff::Initial { entries } => {
                touched.extend(self.left.keys().cloned());
                self.left.clear();
                self.buckets.clear();
                for (key, value) in entries {
                    self.set_left(key, value, extract(key, value), touched);
                }
            }
            MapDiff::Insert { key, value } => {
                self.set_left(key, value, extract(key, value), touched);
            }
            MapDiff::Update { key, new_value, .. } => {
                self.set_left(key, new_value, extract(key, new_value), touched);
            }
            MapDiff::Remove { key, .. } => {
                touched.insert(key.clone());
                if let Some((_, join)) = self.left.remove(key) {
                    self.unindex(key, &join);
                }
            }
            MapDiff::Batch { changes } => {
                for change in changes {
                    self.apply_left(change, extract, touched);
                }
            }
        }
    }
    fn remember(&self, join: &J, previous: &mut MembershipTouched<J>) {
        if let Entry::Vacant(entry) = previous.before.entry(join.clone()) {
            entry.insert(self.counts.contains_key(join));
            previous.order.insert(join.clone());
        }
    }
    fn set_right(&mut self, key: &RK, join: Option<J>, previous: &mut MembershipTouched<J>) {
        let old = self.right.get(key).cloned().flatten();
        if old != join {
            if let Some(old) = old {
                self.remember(&old, previous);
                if let Entry::Occupied(mut entry) = self.counts.entry(old) {
                    if *entry.get() == 1 {
                        entry.remove();
                    } else {
                        *entry.get_mut() = entry.get().saturating_sub(1);
                    }
                }
            }
            if let Some(join) = &join {
                self.remember(join, previous);
                let count = self.counts.entry(join.clone()).or_default();
                *count = count.saturating_add(1);
            }
        }
        self.right.insert(key.clone(), join);
    }
    fn apply_right<RV: CellValue>(
        &mut self,
        diff: &MapDiff<RK, RV>,
        extract: &impl RightJoinKey<RK, RV, J>,
        previous: &mut MembershipTouched<J>,
    ) {
        match diff {
            MapDiff::Initial { entries } => {
                for join in self.counts.keys() {
                    self.remember(join, previous);
                }
                self.right.clear();
                self.counts.clear();
                for (key, value) in entries {
                    self.set_right(key, extract.right_join_key(key, value), previous);
                }
            }
            MapDiff::Insert { key, value } => {
                self.set_right(key, extract.right_join_key(key, value), previous);
            }
            MapDiff::Update { key, new_value, .. } => {
                self.set_right(key, extract.right_join_key(key, new_value), previous);
            }
            MapDiff::Remove { key, .. } => {
                self.set_right(key, None, previous);
                self.right.remove(key);
            }
            MapDiff::Batch { changes } => {
                for change in changes {
                    self.apply_right(change, extract, previous);
                }
            }
        }
    }
    fn publish(&mut self, keys: &mut OrderedSet<LK>) -> Vec<MapDiff<LK, LV>> {
        let mut changes = Vec::new();
        for key in keys.drain() {
            let next = self
                .left
                .get(&key)
                .filter(|(_, join)| self.counts.contains_key(join))
                .map(|(value, _)| value);
            if let Some(next) = next {
                match self.published.entry(key.clone()) {
                    Entry::Occupied(mut entry) => {
                        if entry.get() != next {
                            let old_value = entry.insert(next.clone());
                            changes.push(MapDiff::Update {
                                key,
                                old_value,
                                new_value: next.clone(),
                            });
                        }
                    }
                    Entry::Vacant(entry) => {
                        entry.insert(next.clone());
                        changes.push(MapDiff::Insert {
                            key,
                            value: next.clone(),
                        });
                    }
                }
            } else if let Some(old_value) = self.published.remove(&key) {
                changes.push(MapDiff::Remove { key, old_value });
            }
        }
        changes
    }
}

pub fn install_semi_join_runtime<LK, LV, RK, RV, J, L, R, FL, FR>(
    cx: &mut CompileContext,
    left: L,
    right: R,
    left_key: FL,
    right_key: FR,
    sink: BoxedMapDiffSink<LK, LV>,
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
    // This runtime owns membership counts, so a relation hint must not leak into child plans.
    let _relation = cx.take_relation_hint();
    let state = Arc::new(Mutex::new(State::<LK, LV, RK, J>::default()));
    let left_sink = {
        let state = state.clone();
        let sink = sink.clone();
        move |diff: &MapDiff<LK, LV>| {
            let mut state = state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut touched = OrderedSet::default();
            state.apply_left(diff, &left_key, &mut touched);
            let changes = state.publish(&mut touched);
            // Serialize publication with mutation, including racing sibling roots.
            super::emit_changes(&sink, changes);
            drop(state);
        }
    };
    let right_sink = move |diff: &MapDiff<RK, RV>| {
        let mut state = state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut previous = MembershipTouched::default();
        state.apply_right(diff, &right_key, &mut previous);
        let mut touched = OrderedSet::default();
        for join in previous.order.drain() {
            let existed = previous.before.get(&join).copied().unwrap_or(false);
            if existed != state.counts.contains_key(&join)
                && let Some(bucket) = state.buckets.get(&join)
            {
                touched.extend(bucket.iter().cloned());
            }
        }
        let changes = state.publish(&mut touched);
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
