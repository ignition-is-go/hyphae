use std::{
    collections::{HashMap, HashSet},
    hash::Hash,
    marker::PhantomData,
    sync::{Arc, Mutex},
};

use crate::{
    cell::{CellImmutable, CellMutable},
    cell_map::{CellMap, MapDiff},
    subscription::SubscriptionGuard,
    traits::{CellValue, reactive_map::ReactiveMap},
};

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
    if let MapDiff::Batch { changes } = diff {
        for change in changes {
            apply_source_diff(source_rows, change, impacted);
        }
        return;
    }

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
        MapDiff::Batch { .. } => unreachable!("batch handled above"),
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

        let Some(source_value) = state.source_rows.get(&source_key) else {
            // Fast-path for removes/absent rows that were never projected:
            // no previous output keys means no downstream work at all.
            if previous_output_keys.is_empty() {
                continue;
            }
            for stale_key in previous_output_keys {
                if let Some(old_value) = state.output_cache.remove(&stale_key) {
                    changes.push(MapDiff::Remove {
                        key: stale_key,
                        old_value,
                    });
                }
            }
            continue;
        };

        let mut desired_rows: HashMap<OK, OV> = HashMap::new();
        for (out_key, out_value) in compute_rows(&source_key, source_value) {
            desired_rows.insert(out_key, out_value);
        }

        // If nothing was previously projected and nothing is now projected,
        // skip all downstream bookkeeping.
        if previous_output_keys.is_empty() && desired_rows.is_empty() {
            continue;
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
                    if old_value != new_value {
                        state
                            .output_cache
                            .insert(out_key.clone(), new_value.clone());
                        changes.push(MapDiff::Update {
                            key: out_key,
                            old_value,
                            new_value,
                        });
                    }
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

/// Wrap a non-empty change vector in `MapDiff::Batch`, dropping empty groups.
fn emit_changes<K, V>(changes: Vec<MapDiff<K, V>>, sink: &crate::map_query::MapDiffSink<K, V>)
where
    K: Hash + Eq + CellValue,
    V: CellValue,
{
    if changes.is_empty() {
        return;
    }
    sink(&MapDiff::Batch { changes });
}

/// Install map-runtime machinery that drives `sink` instead of allocating an output map.
///
/// Subscribes to the source, maintains projection state, and emits resulting
/// diffs (batched per upstream diff) into the sink. Returns the subscription
/// guard, which the caller owns.
///
/// Used by `MapQuery` plan nodes (`ProjectPlan`, `ProjectManyPlan`,
/// `SelectPlan`) whose materialization shares one output cell map.
pub(crate) fn install_map_runtime<S, OK, OV, FO>(
    source: &S,
    compute_rows: FO,
    sink: crate::map_query::MapDiffSink<OK, OV>,
) -> Vec<SubscriptionGuard>
where
    S: ReactiveMap,
    S::Key: Hash + Eq + CellValue,
    S::Value: CellValue,
    OK: Hash + Eq + CellValue,
    OV: CellValue,
    FO: Fn(&S::Key, &S::Value) -> Vec<(OK, OV)> + Send + Sync + 'static,
{
    let state = Arc::new(Mutex::new(
        MapState::<S::Key, S::Value, OK, OV>::default(),
    ));
    let compute_rows = Arc::new(compute_rows);

    let guard = source.subscribe_diffs_reactive(move |diff| {
        let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
        let mut impacted: HashSet<S::Key> = HashSet::new();
        apply_source_diff(&mut state.source_rows, diff, &mut impacted);
        let changes = recompute_impacted(&mut state, impacted, compute_rows.as_ref());
        drop(state);
        emit_changes(changes, &sink);
    });

    vec![guard]
}

pub(crate) fn run_map_runtime<SK, SV, SM, OK, OV, FO>(
    source: &CellMap<SK, SV, SM>,
    op_name: &str,
    compute_rows: FO,
) -> CellMap<OK, OV, CellImmutable>
where
    SK: Hash + Eq + CellValue,
    SV: CellValue,
    SM: Send + Sync + 'static,
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

    let weak = Arc::downgrade(&output.inner);
    let sink: crate::map_query::MapDiffSink<OK, OV> = Arc::new(move |diff| {
        let Some(inner) = weak.upgrade() else { return };
        let out = CellMap::<OK, OV, CellMutable> {
            inner,
            _marker: PhantomData,
        };
        out.apply_diff_owned(diff.clone());
    });

    let guards = install_map_runtime(source, compute_rows, sink);
    for g in guards {
        output.own(g);
    }
    output.lock()
}
