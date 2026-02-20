use std::{
    collections::{HashMap, HashSet},
    hash::Hash,
    marker::PhantomData,
    sync::Arc,
};

use arc_swap::ArcSwap;

use super::map_runtime::flatten_diff;
use crate::{
    cell::{CellImmutable, CellMutable},
    cell_map::{CellMap, MapDiff},
    traits::CellValue,
};

pub(crate) fn run_diff_runtime<SK, SV, SM, OK, OV, ST, FS>(
    source: &CellMap<SK, SV, SM>,
    op_name: &str,
    initial_state: ST,
    apply_atomic: FS,
) -> CellMap<OK, OV, CellImmutable>
where
    SK: Hash + Eq + CellValue,
    SV: CellValue,
    OK: Hash + Eq + CellValue,
    OV: CellValue,
    ST: Clone + Send + Sync + 'static,
    FS: Fn(&mut ST, &MapDiff<SK, SV>, &mut Vec<MapDiff<OK, OV>>) + Send + Sync + 'static,
{
    let output = CellMap::<OK, OV, CellMutable>::new();
    if let Some(parent_name) = (**source.inner.name.load()).as_ref() {
        output
            .clone()
            .with_name(format!("{}::{}", parent_name, op_name));
    }

    let state = Arc::new(ArcSwap::from_pointee(initial_state));
    let apply_atomic = Arc::new(apply_atomic);
    let output_weak = Arc::downgrade(&output.inner);

    let guard = source.subscribe_diffs(move |diff| {
        let Some(inner) = output_weak.upgrade() else {
            return;
        };
        let output = CellMap::<OK, OV, CellMutable> {
            inner,
            _marker: PhantomData,
        };

        let mut atomic_diffs: Vec<MapDiff<SK, SV>> = Vec::new();
        flatten_diff(&diff, &mut atomic_diffs);
        let emitted = std::cell::RefCell::new(Vec::<MapDiff<OK, OV>>::new());
        state.rcu(|current| {
            let mut next = current.as_ref().clone();
            {
                let out = &mut *emitted.borrow_mut();
                for atomic in &atomic_diffs {
                    apply_atomic(&mut next, atomic, out);
                }
            }
            next
        });
        output.apply_batch(emitted.into_inner());
    });

    output.own(guard);
    output.lock()
}

#[derive(Clone)]
struct GroupedState<SK, GK, GS>
where
    SK: Hash + Eq + CellValue,
    GK: Hash + Eq + CellValue,
    GS: Clone + Send + Sync + 'static,
{
    source_to_group: HashMap<SK, GK>,
    groups: HashMap<GK, GS>,
}

impl<SK, GK, GS> Default for GroupedState<SK, GK, GS>
where
    SK: Hash + Eq + CellValue,
    GK: Hash + Eq + CellValue,
    GS: Clone + Send + Sync + 'static,
{
    fn default() -> Self {
        Self {
            source_to_group: HashMap::new(),
            groups: HashMap::new(),
        }
    }
}

pub(crate) struct GroupedOps<SK, SV, GK, GS, OV, FG, FI, FU, FR, FM, FE>
where
    SK: Hash + Eq + CellValue,
    SV: CellValue,
    GK: Hash + Eq + CellValue,
    GS: Clone + Send + Sync + 'static,
    OV: CellValue,
    FG: Fn(&SK, &SV) -> GK + Send + Sync + 'static,
    FI: Fn(&mut GS, &SK, &SV) + Send + Sync + 'static,
    FU: Fn(&mut GS, &SK, &SV, &SV) + Send + Sync + 'static,
    FR: Fn(&mut GS, &SK, &SV) + Send + Sync + 'static,
    FM: Fn(&GS) -> OV + Send + Sync + 'static,
    FE: Fn(&GS) -> bool + Send + Sync + 'static,
{
    pub make_group_key: FG,
    pub on_insert: FI,
    pub on_update: FU,
    pub on_remove: FR,
    pub materialize: FM,
    pub is_empty: FE,
    pub new_group_state: fn() -> GS,
    pub _marker: std::marker::PhantomData<fn(&SK, &SV) -> GK>,
}

pub(crate) fn run_grouped_runtime<SK, SV, SM, GK, GS, OV, FG, FI, FU, FR, FM, FE>(
    source: &CellMap<SK, SV, SM>,
    op_name: &str,
    ops: GroupedOps<SK, SV, GK, GS, OV, FG, FI, FU, FR, FM, FE>,
) -> CellMap<GK, OV, CellImmutable>
where
    SK: Hash + Eq + CellValue,
    SV: CellValue,
    GK: Hash + Eq + CellValue,
    GS: Clone + Send + Sync + 'static,
    OV: CellValue,
    FG: Fn(&SK, &SV) -> GK + Send + Sync + 'static,
    FI: Fn(&mut GS, &SK, &SV) + Send + Sync + 'static,
    FU: Fn(&mut GS, &SK, &SV, &SV) + Send + Sync + 'static,
    FR: Fn(&mut GS, &SK, &SV) + Send + Sync + 'static,
    FM: Fn(&GS) -> OV + Send + Sync + 'static,
    FE: Fn(&GS) -> bool + Send + Sync + 'static,
{
    let ops = Arc::new(ops);
    run_diff_runtime(
        source,
        op_name,
        GroupedState::<SK, GK, GS>::default(),
        move |state: &mut GroupedState<SK, GK, GS>,
              diff: &MapDiff<SK, SV>,
              downstream: &mut Vec<MapDiff<GK, OV>>| {
            let mut touched = HashSet::<GK>::new();
            let mut before = HashMap::<GK, Option<OV>>::new();

            let capture_before = |group: &GK,
                                  state: &GroupedState<SK, GK, GS>,
                                  before: &mut HashMap<GK, Option<OV>>,
                                  touched: &mut HashSet<GK>| {
                before.entry(group.clone()).or_insert_with(|| {
                    state
                        .groups
                        .get(group)
                        .map(|group_state| (ops.materialize)(group_state))
                });
                touched.insert(group.clone());
            };

            match diff {
                MapDiff::Initial { entries } => {
                    let old_groups = std::mem::take(&mut state.groups);
                    state.source_to_group.clear();
                    for (group, group_state) in old_groups {
                        before.insert(group.clone(), Some((ops.materialize)(&group_state)));
                        touched.insert(group);
                    }
                    for (key, value) in entries {
                        let group = (ops.make_group_key)(key, value);
                        before.entry(group.clone()).or_insert(None);
                        touched.insert(group.clone());
                        state.source_to_group.insert(key.clone(), group.clone());
                        let group_state = state
                            .groups
                            .entry(group)
                            .or_insert_with(|| (ops.new_group_state)());
                        (ops.on_insert)(group_state, key, value);
                    }
                }
                MapDiff::Insert { key, value } => {
                    let group = (ops.make_group_key)(key, value);
                    capture_before(&group, state, &mut before, &mut touched);
                    state.source_to_group.insert(key.clone(), group.clone());
                    let group_state = state
                        .groups
                        .entry(group)
                        .or_insert_with(|| (ops.new_group_state)());
                    (ops.on_insert)(group_state, key, value);
                }
                MapDiff::Update {
                    key,
                    old_value,
                    new_value,
                } => {
                    let new_group = (ops.make_group_key)(key, new_value);
                    let old_group = state
                        .source_to_group
                        .insert(key.clone(), new_group.clone())
                        .unwrap_or_else(|| (ops.make_group_key)(key, old_value));
                    capture_before(&old_group, state, &mut before, &mut touched);
                    capture_before(&new_group, state, &mut before, &mut touched);

                    if old_group == new_group {
                        let group_state = state
                            .groups
                            .entry(new_group)
                            .or_insert_with(|| (ops.new_group_state)());
                        (ops.on_update)(group_state, key, old_value, new_value);
                    } else {
                        if let Some(group_state) = state.groups.get_mut(&old_group) {
                            (ops.on_remove)(group_state, key, old_value);
                            if (ops.is_empty)(group_state) {
                                state.groups.remove(&old_group);
                            }
                        }
                        let group_state = state
                            .groups
                            .entry(new_group)
                            .or_insert_with(|| (ops.new_group_state)());
                        (ops.on_insert)(group_state, key, new_value);
                    }
                }
                MapDiff::Remove { key, old_value } => {
                    let group = state
                        .source_to_group
                        .remove(key)
                        .unwrap_or_else(|| (ops.make_group_key)(key, old_value));
                    capture_before(&group, state, &mut before, &mut touched);
                    if let Some(group_state) = state.groups.get_mut(&group) {
                        (ops.on_remove)(group_state, key, old_value);
                        if (ops.is_empty)(group_state) {
                            state.groups.remove(&group);
                        }
                    }
                }
                MapDiff::Batch { .. } => {
                    unreachable!("run_diff_runtime flattens MapDiff::Batch before apply_atomic")
                }
            }

            for group in touched {
                let old_value = before.get(&group).cloned().unwrap_or(None);
                let new_value = state
                    .groups
                    .get(&group)
                    .map(|group_state| (ops.materialize)(group_state));
                match (old_value, new_value) {
                    (Some(old_value), Some(new_value)) => downstream.push(MapDiff::Update {
                        key: group.clone(),
                        old_value,
                        new_value,
                    }),
                    (None, Some(new_value)) => downstream.push(MapDiff::Insert {
                        key: group.clone(),
                        value: new_value,
                    }),
                    (Some(old_value), None) => downstream.push(MapDiff::Remove {
                        key: group.clone(),
                        old_value,
                    }),
                    (None, None) => {}
                }
            }
        },
    )
}

#[derive(Clone)]
struct RefcountState<K, V>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    counts: HashMap<K, usize>,
    representative: HashMap<K, V>,
}

impl<K, V> Default for RefcountState<K, V>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    fn default() -> Self {
        Self {
            counts: HashMap::new(),
            representative: HashMap::new(),
        }
    }
}

pub(crate) fn run_refcount_runtime<K, V, M>(
    source: &CellMap<K, V, M>,
    op_name: &str,
) -> CellMap<K, V, CellImmutable>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    run_diff_runtime(
        source,
        op_name,
        RefcountState::<K, V>::default(),
        move |state: &mut RefcountState<K, V>,
              diff: &MapDiff<K, V>,
              downstream: &mut Vec<MapDiff<K, V>>| {
            match diff {
                MapDiff::Initial { entries } => {
                    state.counts.clear();
                    state.representative.clear();
                    for (key, value) in entries {
                        state.counts.insert(key.clone(), 1);
                        state.representative.insert(key.clone(), value.clone());
                        downstream.push(MapDiff::Insert {
                            key: key.clone(),
                            value: value.clone(),
                        });
                    }
                }
                MapDiff::Insert { key, value } => {
                    let count = state.counts.entry(key.clone()).or_insert(0);
                    *count += 1;
                    if *count == 1 {
                        state.representative.insert(key.clone(), value.clone());
                        downstream.push(MapDiff::Insert {
                            key: key.clone(),
                            value: value.clone(),
                        });
                    }
                }
                MapDiff::Update {
                    key,
                    old_value,
                    new_value,
                } => {
                    if state.counts.contains_key(key) {
                        state.representative.insert(key.clone(), new_value.clone());
                        downstream.push(MapDiff::Update {
                            key: key.clone(),
                            old_value: old_value.clone(),
                            new_value: new_value.clone(),
                        });
                    }
                }
                MapDiff::Remove { key, old_value } => {
                    if let Some(count) = state.counts.get_mut(key) {
                        if *count > 1 {
                            *count -= 1;
                        } else {
                            state.counts.remove(key);
                            state.representative.remove(key);
                            downstream.push(MapDiff::Remove {
                                key: key.clone(),
                                old_value: old_value.clone(),
                            });
                        }
                    }
                }
                MapDiff::Batch { .. } => {
                    unreachable!("run_diff_runtime flattens MapDiff::Batch before apply_atomic")
                }
            }
        },
    )
}
