use std::{
    hash::Hash,
    sync::{Arc, Mutex},
};

use rustc_hash::{FxHashMap, FxHashSet};

use super::ordered_set::OrderedSet;
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
    source_order: OrderedSet<SK>,
    source_output_keys: FxHashMap<SK, OrderedSet<OK>>,
    output_owners: FxHashMap<OK, SK>,
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
            source_order: OrderedSet::default(),
            source_output_keys: FxHashMap::default(),
            output_owners: FxHashMap::default(),
            output_cache: FxHashMap::default(),
        }
    }
}

fn apply_source_diff<SK, SV>(
    source_rows: &mut FxHashMap<SK, SV>,
    source_order: &mut OrderedSet<SK>,
    diff: &MapDiff<SK, SV>,
    impacted: &mut OrderedSet<SK>,
) where
    SK: Hash + Eq + CellValue,
    SV: CellValue,
{
    if let MapDiff::Batch { changes } = diff {
        for change in changes {
            apply_source_diff(source_rows, source_order, change, impacted);
        }
        return;
    }

    match diff {
        MapDiff::Initial { entries } => {
            let previous: Vec<SK> = source_order.drain_vec();
            source_rows.clear();
            for key in previous {
                impacted.insert(key);
            }
            for (key, value) in entries {
                source_rows.insert(key.clone(), value.clone());
                source_order.insert(key.clone());
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
            source_order.insert(key.clone());
            impacted.insert(key.clone());
        }
        MapDiff::Remove { key, .. } => {
            source_rows.remove(key);
            source_order.remove(key);
            impacted.insert(key.clone());
        }
        MapDiff::Batch { .. } => {}
    }
}

fn recompute_impacted<SK, SV, OK, OV, FO>(
    state: &mut MapState<SK, SV, OK, OV>,
    mut impacted: OrderedSet<SK>,
    compute_rows: &FO,
) -> Vec<MapDiff<OK, OV>>
where
    SK: Hash + Eq + CellValue,
    SV: CellValue,
    OK: Hash + Eq + CellValue,
    OV: CellValue,
    FO: Fn(&SK, &SV) -> Vec<(OK, OV)>,
{
    let impacted: Vec<SK> = impacted.drain_vec();
    let impacted_set: FxHashSet<SK> = impacted.iter().cloned().collect();
    let mut claimed_outputs = FxHashMap::<OK, SK>::default();
    let mut computed = Vec::with_capacity(impacted.len());

    for source_key in impacted {
        let mut desired_keys = OrderedSet::default();
        let mut desired_rows = Vec::new();
        if let Some(source_value) = state.source_rows.get(&source_key) {
            for (output_key, output_value) in compute_rows(&source_key, source_value) {
                assert!(
                    desired_keys.insert(output_key.clone()),
                    "map query unique-output-key contract violated within one source row"
                );
                let previous_claim = claimed_outputs.insert(output_key.clone(), source_key.clone());
                assert!(
                    previous_claim.is_none(),
                    "map query unique-output-key contract violated across source rows"
                );
                desired_rows.push((output_key, output_value));
            }
        }
        computed.push((source_key, desired_keys, desired_rows));
    }

    for (source_key, _, desired_rows) in &computed {
        for (output_key, _) in desired_rows {
            if let Some(owner) = state.output_owners.get(output_key) {
                assert!(
                    owner == source_key || impacted_set.contains(owner),
                    "map query unique-output-key contract violated by an existing source row"
                );
            }
        }
    }

    let mut changes = Vec::new();
    for (source_key, desired_keys, _) in &computed {
        let previous_keys = state
            .source_output_keys
            .remove(source_key)
            .unwrap_or_default();
        for stale_key in previous_keys
            .iter()
            .filter(|output_key| !desired_keys.contains(*output_key))
        {
            state.output_owners.remove(stale_key);
            if let Some(old_value) = state.output_cache.remove(stale_key) {
                changes.push(MapDiff::Remove {
                    key: stale_key.clone(),
                    old_value,
                });
            }
        }
    }

    for (source_key, desired_keys, desired_rows) in computed {
        for (output_key, new_value) in desired_rows {
            state
                .output_owners
                .insert(output_key.clone(), source_key.clone());
            if let Some(old_value) = state.output_cache.get(&output_key).cloned() {
                if old_value != new_value {
                    state
                        .output_cache
                        .insert(output_key.clone(), new_value.clone());
                    changes.push(MapDiff::Update {
                        key: output_key,
                        old_value,
                        new_value,
                    });
                }
            } else {
                state
                    .output_cache
                    .insert(output_key.clone(), new_value.clone());
                changes.push(MapDiff::Insert {
                    key: output_key,
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

/// Wrap a non-empty change vector in `MapDiff::Batch`, dropping empty groups.
fn emit_changes<K, V>(changes: Vec<MapDiff<K, V>>, sink: &crate::map_query::BoxedMapDiffSink<K, V>)
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
/// Compiles the upstream plan into a direct entry point, maintains projection
/// state, and emits resulting diffs (batched per upstream
/// diff) into the sink. Returns the subscription guards, which the caller owns.
///
/// Used by `MapQuery` plan nodes (`ProjectPlan`, `ProjectManyPlan`,
/// `SelectPlan`) whose materialization shares one output cell map. Chains of
/// plans compose without intermediate [`CellMap`](crate::CellMap) allocations.
pub fn install_map_runtime_via_query<SK, SV, OK, OV, S, FO>(
    cx: &mut crate::map_query::compiler::CompileContext,
    source: S,
    compute_rows: FO,
    sink: crate::map_query::BoxedMapDiffSink<OK, OV>,
) -> Vec<SubscriptionGuard>
where
    SK: Hash + Eq + CellValue,
    SV: CellValue,
    OK: Hash + Eq + CellValue,
    OV: CellValue,
    S: crate::map_query::MapQuery<Key = SK, Value = SV>,
    FO: Fn(&SK, &SV) -> Vec<(OK, OV)> + Send + Sync + 'static,
{
    let state = Arc::new(Mutex::new(MapState::<SK, SV, OK, OV>::default()));
    let upstream_sink = {
        move |diff: &MapDiff<SK, SV>| {
            let mut state = state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut impacted = OrderedSet::default();
            let mut source_order = std::mem::take(&mut state.source_order);
            apply_source_diff(
                &mut state.source_rows,
                &mut source_order,
                diff,
                &mut impacted,
            );
            state.source_order = source_order;
            let changes = recompute_impacted(&mut state, impacted, &compute_rows);
            drop(state);
            emit_changes(changes, &sink);
        }
    };

    crate::map_query::compile_runtime_into(source, cx, Arc::new(upstream_sink))
}

#[cfg(test)]
mod single_runtime_comparison {
    use super::*;

    fn replay(diff: &MapDiff<u32, i32>, output: &mut FxHashMap<u32, i32>) {
        match diff {
            MapDiff::Initial { entries } => {
                output.clear();
                output.extend(entries.iter().copied());
            }
            MapDiff::Insert { key, value } => {
                assert!(output.insert(*key, *value).is_none());
            }
            MapDiff::Update {
                key,
                old_value,
                new_value,
            } => {
                assert_eq!(output.insert(*key, *new_value), Some(*old_value));
            }
            MapDiff::Remove { key, old_value } => {
                assert_eq!(output.remove(key), Some(*old_value));
            }
            MapDiff::Batch { changes } => {
                for change in changes {
                    replay(change, output);
                }
            }
        }
    }

    #[test]
    fn single_projection_matches_general_runtime_for_mixed_transactions() {
        let mut general = MapState::<u32, i32, u32, i32>::default();
        let mut single = super::super::single_runtime::State::<u32, u32, i32>::default();
        let mut general_output = FxHashMap::default();
        let mut single_output = FxHashMap::default();
        let compute = |key: &u32, value: &i32| {
            (*value % 5 != 0).then_some((key * 3 + value.unsigned_abs() % 3, *value))
        };
        let mut rng = 7_u16;
        for step in 0..512 {
            rng = rng.wrapping_mul(25_173).wrapping_add(13_849);
            let key = u32::from(rng % 16);
            let value = i32::from(rng % 100);
            let diff = if step % 31 == 0 {
                MapDiff::Batch {
                    changes: vec![
                        MapDiff::Insert { key, value },
                        MapDiff::Initial {
                            entries: vec![(key, value + 1)],
                        },
                    ],
                }
            } else if step % 7 == 0 {
                MapDiff::Remove {
                    key,
                    old_value: value,
                }
            } else {
                MapDiff::Batch {
                    changes: vec![
                        MapDiff::Insert { key, value },
                        MapDiff::Batch {
                            changes: vec![MapDiff::Insert {
                                key,
                                value: value + 2,
                            }],
                        },
                    ],
                }
            };
            let mut impacted = OrderedSet::default();
            apply_source_diff(
                &mut general.source_rows,
                &mut general.source_order,
                &diff,
                &mut impacted,
            );
            let old_changes = recompute_impacted(&mut general, impacted, &|k, v| {
                compute(k, v).into_iter().collect()
            });
            let new_changes = single.apply(&diff, &compute);
            assert_eq!(
                new_changes, old_changes,
                "diff sequence in transaction {step}"
            );
            for change in &old_changes {
                replay(change, &mut general_output);
            }
            for change in &new_changes {
                replay(change, &mut single_output);
            }
            assert_eq!(single_output, general_output, "transaction {step}");
        }
    }
}
