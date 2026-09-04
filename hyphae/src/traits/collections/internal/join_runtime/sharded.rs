use std::hash::{Hash, Hasher};

use rustc_hash::{FxHashMap, FxHasher};

use crate::{
    cell_map::MapDiff,
    traits::{CellValue, RightJoinKey},
};

use super::state::{
    JoinState, RelationIndexStorage, apply_left_diff, apply_right_diff, recompute_keyed_impacted,
};

#[allow(clippy::missing_const_for_fn)]
pub(super) fn query_shard_count() -> usize {
    #[cfg(all(feature = "scheduler", not(target_arch = "wasm32")))]
    if let Some(pool) = crate::executor::worker_pool() {
        return pool.current_num_threads().max(1);
    }
    1
}

fn shard_for<T: Hash>(value: &T, shard_count: usize) -> usize {
    let mut hasher = FxHasher::default();
    value.hash(&mut hasher);
    let count = u64::try_from(shard_count.max(1)).unwrap_or(1);
    let index = hasher.finish().checked_rem(count).unwrap_or(0);
    usize::try_from(index).unwrap_or(0)
}

const fn diff_merge_phase<K, V>(diff: &MapDiff<K, V>) -> u8 {
    match diff {
        MapDiff::Initial { .. } => 0,
        MapDiff::Remove { .. } => 1,
        MapDiff::Update { .. } => 2,
        MapDiff::Insert { .. } => 3,
        MapDiff::Batch { .. } => 4,
    }
}

fn push_routed<T>(routed: &mut [Vec<T>], route: usize, value: T) {
    debug_assert!(route < routed.len(), "join shard route must be in bounds");
    if let Some(shard) = routed.get_mut(route) {
        shard.push(value);
    }
}

pub(super) struct ShardedKeyedJoin<LK, LV, RK, RV, JK, OV>
where
    LK: Hash + Eq + CellValue,
    LV: CellValue,
    RK: Hash + Eq + CellValue,
    RV: CellValue,
    JK: Hash + Eq + CellValue,
    OV: CellValue,
{
    shards: Vec<JoinState<LK, LV, RK, RV, JK, LK, OV>>,
    left_routes: FxHashMap<LK, usize>,
    right_routes: FxHashMap<RK, usize>,
    left_sequence: FxHashMap<LK, u64>,
    next_sequence: u64,
}

impl<LK, LV, RK, RV, JK, OV> ShardedKeyedJoin<LK, LV, RK, RV, JK, OV>
where
    LK: Hash + Eq + CellValue,
    LV: CellValue,
    RK: Hash + Eq + CellValue,
    RV: CellValue,
    JK: Hash + Eq + CellValue,
    OV: CellValue,
{
    pub(super) fn new(shard_count: usize) -> Self {
        Self {
            shards: (0..shard_count.max(1))
                .map(|_| JoinState::default())
                .collect(),
            left_routes: FxHashMap::default(),
            right_routes: FxHashMap::default(),
            left_sequence: FxHashMap::default(),
            next_sequence: 0,
        }
    }

    pub(super) fn route_left<FL>(
        &mut self,
        diff: &MapDiff<LK, LV>,
        left_join_key: &FL,
    ) -> (Vec<Vec<MapDiff<LK, LV>>>, FxHashMap<LK, u64>)
    where
        FL: Fn(&LK, &LV) -> JK,
    {
        let mut flattened = Vec::new();
        crate::traits::collections::internal::map_runtime::flatten_diff(diff, &mut flattened);
        self.route_left_owned(flattened, left_join_key)
    }

    #[allow(clippy::too_many_lines)]
    pub(super) fn route_left_owned<FL>(
        &mut self,
        flattened: Vec<MapDiff<LK, LV>>,
        left_join_key: &FL,
    ) -> (Vec<Vec<MapDiff<LK, LV>>>, FxHashMap<LK, u64>)
    where
        FL: Fn(&LK, &LV) -> JK,
    {
        let mut routed = (0..self.shards.len())
            .map(|_| Vec::new())
            .collect::<Vec<_>>();
        let mut event_ordinals = FxHashMap::default();

        for (event_index, change) in flattened.into_iter().enumerate() {
            let event_ordinal = u64::try_from(event_index).unwrap_or(u64::MAX);
            match change {
                MapDiff::Initial { entries } => {
                    for shard in &mut routed {
                        shard.push(MapDiff::Initial {
                            entries: Vec::new(),
                        });
                    }
                    self.left_routes.clear();
                    self.left_sequence.clear();
                    self.next_sequence = 0;
                    for (key, value) in entries {
                        let join_key = left_join_key(&key, &value);
                        let route = shard_for(&join_key, self.shards.len());
                        push_routed(
                            &mut routed,
                            route,
                            MapDiff::Insert {
                                key: key.clone(),
                                value,
                            },
                        );
                        self.left_routes.insert(key.clone(), route);
                        self.left_sequence.insert(key.clone(), self.next_sequence);
                        event_ordinals.insert(key, self.next_sequence);
                        self.next_sequence = self.next_sequence.wrapping_add(1);
                    }
                }
                MapDiff::Insert { key, value } => {
                    let join_key = left_join_key(&key, &value);
                    let route = shard_for(&join_key, self.shards.len());
                    push_routed(
                        &mut routed,
                        route,
                        MapDiff::Insert {
                            key: key.clone(),
                            value,
                        },
                    );
                    self.left_routes.insert(key.clone(), route);
                    if !self.left_sequence.contains_key(&key) {
                        self.left_sequence.insert(key.clone(), self.next_sequence);
                        self.next_sequence = self.next_sequence.wrapping_add(1);
                    }
                    event_ordinals.entry(key).or_insert(event_ordinal);
                }
                MapDiff::Update {
                    key,
                    old_value,
                    new_value,
                } => {
                    let new_join_key = left_join_key(&key, &new_value);
                    let new_route = shard_for(&new_join_key, self.shards.len());
                    let old_route = self.left_routes.get(&key).copied().unwrap_or_else(|| {
                        shard_for(&left_join_key(&key, &old_value), self.shards.len())
                    });
                    if old_route == new_route {
                        push_routed(
                            &mut routed,
                            new_route,
                            MapDiff::Update {
                                key: key.clone(),
                                old_value,
                                new_value,
                            },
                        );
                    } else {
                        push_routed(
                            &mut routed,
                            old_route,
                            MapDiff::Remove {
                                key: key.clone(),
                                old_value,
                            },
                        );
                        push_routed(
                            &mut routed,
                            new_route,
                            MapDiff::Insert {
                                key: key.clone(),
                                value: new_value,
                            },
                        );
                    }
                    self.left_routes.insert(key.clone(), new_route);
                    event_ordinals.entry(key).or_insert(event_ordinal);
                }
                MapDiff::Remove { key, old_value } => {
                    let route = self.left_routes.remove(&key).unwrap_or_else(|| {
                        shard_for(&left_join_key(&key, &old_value), self.shards.len())
                    });
                    push_routed(
                        &mut routed,
                        route,
                        MapDiff::Remove {
                            key: key.clone(),
                            old_value,
                        },
                    );
                    self.left_sequence.remove(&key);
                    event_ordinals.entry(key).or_insert(event_ordinal);
                }
                MapDiff::Batch { .. } => {}
            }
        }
        (routed, event_ordinals)
    }

    #[allow(clippy::too_many_lines)]
    pub(super) fn route_right<FR>(
        &mut self,
        diff: &MapDiff<RK, RV>,
        right_join_key: &FR,
    ) -> Vec<Vec<MapDiff<RK, RV>>>
    where
        FR: RightJoinKey<RK, RV, JK>,
    {
        let mut routed = (0..self.shards.len())
            .map(|_| Vec::new())
            .collect::<Vec<_>>();
        let mut flattened = Vec::new();
        crate::traits::collections::internal::map_runtime::flatten_diff(diff, &mut flattened);

        for change in flattened {
            match change {
                MapDiff::Initial { entries } => {
                    for shard in &mut routed {
                        shard.push(MapDiff::Initial {
                            entries: Vec::new(),
                        });
                    }
                    self.right_routes.clear();
                    for (key, value) in entries {
                        if let Some(join_key) = right_join_key.right_join_key(&key, &value) {
                            let route = shard_for(&join_key, self.shards.len());
                            push_routed(
                                &mut routed,
                                route,
                                MapDiff::Insert {
                                    key: key.clone(),
                                    value,
                                },
                            );
                            self.right_routes.insert(key, route);
                        }
                    }
                }
                MapDiff::Insert { key, value } => {
                    if let Some(join_key) = right_join_key.right_join_key(&key, &value) {
                        let route = shard_for(&join_key, self.shards.len());
                        push_routed(
                            &mut routed,
                            route,
                            MapDiff::Insert {
                                key: key.clone(),
                                value,
                            },
                        );
                        self.right_routes.insert(key, route);
                    }
                }
                MapDiff::Update {
                    key,
                    old_value,
                    new_value,
                } => {
                    let old_route = self.right_routes.remove(&key).or_else(|| {
                        right_join_key
                            .right_join_key(&key, &old_value)
                            .map(|join_key| shard_for(&join_key, self.shards.len()))
                    });
                    let new_route = right_join_key
                        .right_join_key(&key, &new_value)
                        .map(|join_key| shard_for(&join_key, self.shards.len()));
                    match (old_route, new_route) {
                        (Some(old_route), Some(new_route)) if old_route == new_route => {
                            push_routed(
                                &mut routed,
                                new_route,
                                MapDiff::Update {
                                    key: key.clone(),
                                    old_value,
                                    new_value,
                                },
                            );
                            self.right_routes.insert(key, new_route);
                        }
                        (Some(old_route), Some(new_route)) => {
                            push_routed(
                                &mut routed,
                                old_route,
                                MapDiff::Remove {
                                    key: key.clone(),
                                    old_value,
                                },
                            );
                            push_routed(
                                &mut routed,
                                new_route,
                                MapDiff::Insert {
                                    key: key.clone(),
                                    value: new_value,
                                },
                            );
                            self.right_routes.insert(key, new_route);
                        }
                        (Some(old_route), None) => {
                            push_routed(&mut routed, old_route, MapDiff::Remove { key, old_value });
                        }
                        (None, Some(new_route)) => {
                            push_routed(
                                &mut routed,
                                new_route,
                                MapDiff::Insert {
                                    key: key.clone(),
                                    value: new_value,
                                },
                            );
                            self.right_routes.insert(key, new_route);
                        }
                        (None, None) => {}
                    }
                }
                MapDiff::Remove { key, old_value } => {
                    let route = self.right_routes.remove(&key).or_else(|| {
                        right_join_key
                            .right_join_key(&key, &old_value)
                            .map(|join_key| shard_for(&join_key, self.shards.len()))
                    });
                    if let Some(route) = route {
                        push_routed(&mut routed, route, MapDiff::Remove { key, old_value });
                    }
                }
                MapDiff::Batch { .. } => {}
            }
        }
        routed
    }

    pub(super) fn merge_changes(
        &self,
        shard_changes: Vec<Vec<MapDiff<LK, OV>>>,
        event_ordinals: &FxHashMap<LK, u64>,
    ) -> Vec<MapDiff<LK, OV>> {
        let mut tagged = Vec::new();
        for changes in shard_changes {
            for (local_ordinal, change) in changes.into_iter().enumerate() {
                let ordinal = change
                    .atomic_key()
                    .and_then(|key| {
                        event_ordinals
                            .get(key)
                            .or_else(|| self.left_sequence.get(key))
                    })
                    .copied()
                    .unwrap_or(u64::MAX);
                let phase = diff_merge_phase(&change);
                tagged.push((ordinal, phase, local_ordinal, change));
            }
        }
        tagged.sort_by_key(|(ordinal, phase, local, _)| (*ordinal, *phase, *local));
        tagged.into_iter().map(|(_, _, _, change)| change).collect()
    }
}

pub(super) fn process_left_shards<LK, LV, RK, RV, JK, OV, FL, FO>(
    join: &mut ShardedKeyedJoin<LK, LV, RK, RV, JK, OV>,
    routed: &[Vec<MapDiff<LK, LV>>],
    left_join_key: &FL,
    compute_value: &FO,
) -> Vec<Vec<MapDiff<LK, OV>>>
where
    LK: Hash + Eq + CellValue,
    LV: CellValue,
    RK: Hash + Eq + CellValue,
    RV: CellValue,
    JK: Hash + Eq + CellValue,
    OV: CellValue,
    FL: Fn(&LK, &LV) -> JK + Sync,
    FO: Fn(&LK, &LV, &[(RK, RV)]) -> OV + Sync,
{
    let process = |(state, diffs): (
        &mut JoinState<LK, LV, RK, RV, JK, LK, OV>,
        &Vec<MapDiff<LK, LV>>,
    )| {
        let mut scratch = std::mem::take(&mut state.scratch);
        for diff in diffs {
            apply_left_diff(state, diff, left_join_key, &mut scratch.impacted);
        }
        let changes = recompute_keyed_impacted(state, &mut scratch, &|key, value, rights| {
            Some(compute_value(key, value, rights))
        });
        state.scratch = scratch;
        changes
    };
    process_join_shards(&mut join.shards, routed, process)
}

pub(super) fn process_right_shards<LK, LV, RK, RV, JK, OV, FR, FO>(
    join: &mut ShardedKeyedJoin<LK, LV, RK, RV, JK, OV>,
    routed: &[Vec<MapDiff<RK, RV>>],
    right_join_key: &FR,
    compute_value: &FO,
) -> Vec<Vec<MapDiff<LK, OV>>>
where
    LK: Hash + Eq + CellValue,
    LV: CellValue,
    RK: Hash + Eq + CellValue,
    RV: CellValue,
    JK: Hash + Eq + CellValue,
    OV: CellValue,
    FR: RightJoinKey<RK, RV, JK> + Sync,
    FO: Fn(&LK, &LV, &[(RK, RV)]) -> OV + Sync,
{
    let process = |(state, diffs): (
        &mut JoinState<LK, LV, RK, RV, JK, LK, OV>,
        &Vec<MapDiff<RK, RV>>,
    )| {
        let mut scratch = std::mem::take(&mut state.scratch);
        for diff in diffs {
            apply_right_diff(
                state,
                diff,
                right_join_key,
                &mut scratch.impacted,
                &mut scratch.changed_join_keys,
            );
        }
        let changes = recompute_keyed_impacted(state, &mut scratch, &|key, value, rights| {
            Some(compute_value(key, value, rights))
        });
        state.scratch = scratch;
        changes
    };
    process_join_shards(&mut join.shards, routed, process)
}

fn process_join_shards<State, Diff, Change, F>(
    shards: &mut [State],
    routed: &[Vec<Diff>],
    process: F,
) -> Vec<Vec<Change>>
where
    State: Send,
    Diff: Sync,
    Change: Send,
    F: Fn((&mut State, &Vec<Diff>)) -> Vec<Change> + Send + Sync,
{
    let work = routed
        .iter()
        .fold(0_usize, |sum, diffs| sum.saturating_add(diffs.len()));
    #[cfg(all(feature = "scheduler", not(target_arch = "wasm32")))]
    if work >= 8_192
        && shards.len() > 1
        && let Some(pool) = crate::executor::worker_pool()
    {
        use rayon::prelude::*;
        return pool.install(|| {
            shards
                .par_iter_mut()
                .zip(routed.par_iter())
                .map(&process)
                .collect()
        });
    }

    let _ = work;
    shards.iter_mut().zip(routed).map(process).collect()
}

pub(super) fn state_left_entries<LK, LV, RK, RV, JK, OK, OV>(
    state: &JoinState<LK, LV, RK, RV, JK, OK, OV, impl RelationIndexStorage<RK, RV, JK>>,
) -> Vec<(LK, LV)>
where
    LK: Hash + Eq + CellValue,
    LV: CellValue,
    RK: Hash + Eq + CellValue,
    RV: CellValue,
    JK: Hash + Eq + CellValue,
    OK: Hash + Eq + CellValue,
    OV: CellValue,
{
    state
        .join_to_left
        .values()
        .flatten()
        .filter_map(|key| {
            state
                .left_rows
                .get(key)
                .map(|value| (key.clone(), value.clone()))
        })
        .collect()
}

pub(super) fn state_right_entries<LK, LV, RK, RV, JK, OK, OV>(
    state: &JoinState<LK, LV, RK, RV, JK, OK, OV, impl RelationIndexStorage<RK, RV, JK>>,
) -> Vec<(RK, RV)>
where
    LK: Hash + Eq + CellValue,
    LV: CellValue,
    RK: Hash + Eq + CellValue,
    RV: CellValue,
    JK: Hash + Eq + CellValue,
    OK: Hash + Eq + CellValue,
    OV: CellValue,
{
    let right = state.right.acquire_read();
    right
        .join_to_rows
        .values()
        .flatten()
        .filter_map(|key| {
            right
                .rows
                .get(key)
                .map(|value| (key.clone(), value.clone()))
        })
        .collect()
}
