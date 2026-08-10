use std::{
    hash::Hash,
    sync::{Arc, Mutex},
};

use rustc_hash::{FxHashMap, FxHashSet};

use crate::{cell_map::MapDiff, subscription::SubscriptionGuard, traits::CellValue};

// Internal projection state — keys are workspace-trusted (entity IDs, etc.),
// so we use FxHash for ~2-3× faster hashing than std's SipHash13.
struct MapState<SK, SV, OK, OV>
where
    SK: Hash + Eq + CellValue,
    SV: CellValue,
    OK: Hash + Eq + CellValue,
    OV: CellValue,
{
    source_rows: FxHashMap<SK, SV>,
    source_output_keys: FxHashMap<SK, FxHashSet<OK>>,
    output_cache: FxHashMap<OK, OV>,
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
            source_rows: FxHashMap::default(),
            source_output_keys: FxHashMap::default(),
            output_cache: FxHashMap::default(),
        }
    }
}

fn apply_source_diff<SK, SV>(
    source_rows: &mut FxHashMap<SK, SV>,
    diff: &MapDiff<SK, SV>,
    impacted: &mut FxHashSet<SK>,
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
        MapDiff::Batch { .. } => {}
    }
}

fn recompute_impacted<SK, SV, OK, OV, FO>(
    state: &mut MapState<SK, SV, OK, OV>,
    impacted: FxHashSet<SK>,
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

        let mut desired_rows: FxHashMap<OK, OV> = FxHashMap::default();
        for (out_key, out_value) in compute_rows(&source_key, source_value) {
            desired_rows.insert(out_key, out_value);
        }

        // If nothing was previously projected and nothing is now projected,
        // skip all downstream bookkeeping.
        if previous_output_keys.is_empty() && desired_rows.is_empty() {
            continue;
        }

        let desired_keys: FxHashSet<OK> = desired_rows.keys().cloned().collect();

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
            if let Some(old_value) = state.output_cache.get(&out_key).cloned() {
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
            } else {
                state
                    .output_cache
                    .insert(out_key.clone(), new_value.clone());
                changes.push(MapDiff::Insert {
                    key: out_key,
                    value: new_value,
                });
            }
        }

        if !desired_keys.is_empty() {
            state.source_output_keys.insert(source_key, desired_keys);
        }
    }

    changes
}

pub fn flatten_diff<K, V>(diff: &MapDiff<K, V>, out: &mut Vec<MapDiff<K, V>>)
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
fn emit_changes<K, V, Sink>(changes: Vec<MapDiff<K, V>>, sink: &Sink)
where
    K: Hash + Eq + CellValue,
    V: CellValue,
    Sink: crate::map_query::MapDiffSink<K, V>,
{
    if changes.is_empty() {
        return;
    }
    sink(&MapDiff::Batch { changes });
}

/// Install map-runtime machinery that drives `sink` instead of allocating an output map.
///
/// Subscribes upstream via [`MapQuery::install`](crate::map_query::MapQuery::install),
/// maintains projection state, and emits resulting diffs (batched per upstream
/// diff) into the sink. Returns the subscription guards, which the caller owns.
///
/// Used by `MapQuery` plan nodes (`ProjectPlan`, `ProjectManyPlan`,
/// `SelectPlan`) whose materialization shares one output cell map. Chains of
/// plans compose without intermediate [`CellMap`](crate::CellMap) allocations.
pub fn install_map_runtime_via_query<SK, SV, OK, OV, S, FO, Sink>(
    source: S,
    compute_rows: FO,
    sink: Sink,
) -> Vec<SubscriptionGuard>
where
    SK: Hash + Eq + CellValue,
    SV: CellValue,
    OK: Hash + Eq + CellValue,
    OV: CellValue,
    S: crate::map_query::MapQuery<Key = SK, Value = SV>,
    FO: Fn(&SK, &SV) -> Vec<(OK, OV)> + Send + Sync + 'static,
    Sink: crate::map_query::MapDiffSink<OK, OV>,
{
    let state = Arc::new(Mutex::new(MapState::<SK, SV, OK, OV>::default()));
    let upstream_sink = {
        move |diff: &MapDiff<SK, SV>| {
            let mut state = state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut impacted: FxHashSet<SK> = FxHashSet::default();
            apply_source_diff(&mut state.source_rows, diff, &mut impacted);
            let changes = recompute_impacted(&mut state, impacted, &compute_rows);
            drop(state);
            emit_changes(changes, &sink);
        }
    };

    source.install(upstream_sink)
}
