use std::{
    collections::{HashMap, HashSet},
    hash::Hash,
    marker::PhantomData,
    sync::Arc,
};

use arc_swap::ArcSwap;

use crate::{
    cell::{CellImmutable, CellMutable},
    cell_map::{CellMap, MapDiff},
    traits::CellValue,
};

#[derive(Clone)]
struct MapState<SK, SV, OK, OV>
where
    SK: Hash + Eq + CellValue,
    SV: CellValue,
    OK: Hash + Eq + CellValue,
    OV: CellValue,
{
    source_rows: HashMap<SK, SV>,
    source_output_keys: HashMap<SK, HashSet<OK>>,
    output_cache: HashMap<OK, OV>,
}

impl<SK, SV, OK, OV> Default for MapState<SK, SV, OK, OV>
where
    SK: Hash + Eq + CellValue,
    SV: CellValue,
    OK: Hash + Eq + CellValue,
    OV: CellValue,
{
    fn default() -> Self {
        Self {
            source_rows: HashMap::new(),
            source_output_keys: HashMap::new(),
            output_cache: HashMap::new(),
        }
    }
}

fn apply_source_diff<SK, SV>(
    source_rows: &mut HashMap<SK, SV>,
    diff: &MapDiff<SK, SV>,
    impacted: &mut HashSet<SK>,
) where
    SK: Hash + Eq + CellValue,
    SV: CellValue,
{
    match diff {
        MapDiff::Initial { entries } => {
            let previous: Vec<SK> = source_rows.keys().cloned().collect();
            source_rows.clear();
            for key in previous {
                impacted.insert(key);
            }
            for (key, value) in entries {
                source_rows.insert(key.clone(), value.clone());
                impacted.insert(key.clone());
            }
        }
        MapDiff::Insert { key, value }
        | MapDiff::Update {
            key,
            new_value: value,
            ..
        } => {
            source_rows.insert(key.clone(), value.clone());
            impacted.insert(key.clone());
        }
        MapDiff::Remove { key, .. } => {
            source_rows.remove(key);
            impacted.insert(key.clone());
        }
        MapDiff::Batch { changes } => {
            for change in changes {
                apply_source_diff(source_rows, change, impacted);
            }
        }
    }
}

fn recompute_impacted<SK, SV, OK, OV, FO>(
    state: &mut MapState<SK, SV, OK, OV>,
    impacted: HashSet<SK>,
    compute_rows: &FO,
) -> Vec<MapDiff<OK, OV>>
where
    SK: Hash + Eq + CellValue,
    SV: CellValue,
    OK: Hash + Eq + CellValue,
    OV: CellValue,
    FO: Fn(&SK, &SV) -> Vec<(OK, OV)>,
{
    let mut changes: Vec<MapDiff<OK, OV>> = Vec::new();

    for source_key in impacted {
        let previous_output_keys = state
            .source_output_keys
            .remove(&source_key)
            .unwrap_or_default();

        let mut desired_rows: HashMap<OK, OV> = HashMap::new();
        if let Some(source_value) = state.source_rows.get(&source_key) {
            for (out_key, out_value) in compute_rows(&source_key, source_value) {
                desired_rows.insert(out_key, out_value);
            }
        }

        let desired_keys: HashSet<OK> = desired_rows.keys().cloned().collect();

        for stale_key in previous_output_keys
            .iter()
            .filter(|output_key| !desired_keys.contains(*output_key))
        {
            if let Some(old_value) = state.output_cache.remove(stale_key) {
                changes.push(MapDiff::Remove {
                    key: stale_key.clone(),
                    old_value,
                });
            }
        }

        for (out_key, new_value) in desired_rows {
            match state.output_cache.get(&out_key).cloned() {
                Some(old_value) => {
                    state
                        .output_cache
                        .insert(out_key.clone(), new_value.clone());
                    changes.push(MapDiff::Update {
                        key: out_key,
                        old_value,
                        new_value,
                    });
                }
                None => {
                    state
                        .output_cache
                        .insert(out_key.clone(), new_value.clone());
                    changes.push(MapDiff::Insert {
                        key: out_key,
                        value: new_value,
                    });
                }
            }
        }

        if !desired_keys.is_empty() {
            state.source_output_keys.insert(source_key, desired_keys);
        }
    }

    changes
}

pub(crate) fn flatten_diff<K, V>(diff: &MapDiff<K, V>, out: &mut Vec<MapDiff<K, V>>)
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    match diff {
        MapDiff::Batch { changes } => {
            for change in changes {
                flatten_diff(change, out);
            }
        }
        _ => out.push(diff.clone()),
    }
}

pub(crate) fn run_map_runtime<SK, SV, SM, OK, OV, FO>(
    source: &CellMap<SK, SV, SM>,
    op_name: &str,
    compute_rows: FO,
) -> CellMap<OK, OV, CellImmutable>
where
    SK: Hash + Eq + CellValue,
    SV: CellValue,
    OK: Hash + Eq + CellValue,
    OV: CellValue,
    FO: Fn(&SK, &SV) -> Vec<(OK, OV)> + Send + Sync + 'static,
{
    let output = CellMap::<OK, OV, CellMutable>::new();
    if let Some(parent_name) = (**source.inner.name.load()).as_ref() {
        output
            .clone()
            .with_name(format!("{}::{}", parent_name, op_name));
    }

    let state = Arc::new(ArcSwap::from_pointee(MapState::<SK, SV, OK, OV>::default()));
    let compute_rows = Arc::new(compute_rows);
    let output_weak = Arc::downgrade(&output.inner);

    let guard = source.subscribe_diffs(move |diff| {
        let Some(inner) = output_weak.upgrade() else {
            return;
        };
        let output = CellMap::<OK, OV, CellMutable> {
            inner,
            _marker: PhantomData,
        };

        let changes_cell = std::cell::RefCell::new(Vec::<MapDiff<OK, OV>>::new());
        state.rcu(|current| {
            let mut next = current.as_ref().clone();
            let mut impacted: HashSet<SK> = HashSet::new();
            apply_source_diff(&mut next.source_rows, &diff, &mut impacted);
            let changes = recompute_impacted(&mut next, impacted, compute_rows.as_ref());
            *changes_cell.borrow_mut() = changes;
            next
        });
        let changes = changes_cell.into_inner();
        output.apply_batch(changes);
    });

    output.own(guard);
    output.lock()
}
