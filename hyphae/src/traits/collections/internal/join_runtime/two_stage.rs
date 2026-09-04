use std::{collections::hash_map::Entry, hash::Hash, sync::Arc};

use rustc_hash::FxHashMap;

use crate::{
    cell_map::MapDiff,
    subscription::SubscriptionGuard,
    traits::{CellValue, RightJoinKey},
};

use super::super::join_lifecycle::{
    BatchedChanges, InstallRegionRights, LegacyTransaction, RegionHost, RootRegistrationOrder,
    RuntimeStorage, install_region_runtime,
};
use super::{
    sharded::{
        ShardedKeyedJoin, process_left_shards, process_right_shards, query_shard_count,
        state_left_entries, state_right_entries,
    },
    state::{
        JoinState, RelationIndexStorage, add_index_member, apply_left_diff, apply_right_diff,
        commit_keyed_value, recompute_keyed_impacted, remove_index_member, remove_left,
        upsert_left,
    },
};

type TwoFirstState<LK, LV, RK, RV, JK, MV> = JoinState<LK, LV, RK, RV, JK, LK, MV>;
type TwoSecondState<LK, MV, RK, RV, JK, OV> = JoinState<LK, MV, RK, RV, JK, LK, OV>;
type TwoFirstShards<LK, LV, RK, RV, JK, MV> = ShardedKeyedJoin<LK, LV, RK, RV, JK, MV>;
type TwoSecondShards<LK, MV, RK, RV, JK, OV> = ShardedKeyedJoin<LK, MV, RK, RV, JK, OV>;

struct TwoKeyedSerial<First, Second> {
    first: First,
    second: Second,
}

struct TwoKeyedSharded<First, Second> {
    first: First,
    second: Second,
}

struct TwoStageConfig<FL1, FR1, FM1, FL2, FR2, FM2> {
    left_key1: Arc<FL1>,
    right_key1: Arc<FR1>,
    project1: Arc<FM1>,
    left_key2: Arc<FL2>,
    right_key2: Arc<FR2>,
    project2: Arc<FM2>,
}

#[allow(clippy::type_complexity)]
struct TwoStageKernel<LK, LV, RK1, RV1, JK1, MV, RK2, RV2, JK2, OV, FL1, FR1, FM1, FL2, FR2, FM2>
where
    LK: Hash + Eq + CellValue,
    LV: CellValue,
    RK1: Hash + Eq + CellValue,
    RV1: CellValue,
    JK1: Hash + Eq + CellValue,
    MV: CellValue,
    RK2: Hash + Eq + CellValue,
    RV2: CellValue,
    JK2: Hash + Eq + CellValue,
    OV: CellValue,
{
    storage: RuntimeStorage<
        TwoKeyedSerial<
            TwoFirstState<LK, LV, RK1, RV1, JK1, MV>,
            TwoSecondState<LK, MV, RK2, RV2, JK2, OV>,
        >,
        TwoKeyedSharded<
            TwoFirstShards<LK, LV, RK1, RV1, JK1, MV>,
            TwoSecondShards<LK, MV, RK2, RV2, JK2, OV>,
        >,
    >,
    config: TwoStageConfig<FL1, FR1, FM1, FL2, FR2, FM2>,
    shard_count: usize,
}

struct TwoStageRights<First, Second> {
    first: First,
    second: Second,
}

#[allow(clippy::too_many_lines, clippy::type_complexity)]
impl<LK, LV, RK1, RV1, JK1, MV, RK2, RV2, JK2, OV, FL1, FR1, FM1, FL2, FR2, FM2>
    TwoStageKernel<LK, LV, RK1, RV1, JK1, MV, RK2, RV2, JK2, OV, FL1, FR1, FM1, FL2, FR2, FM2>
where
    LK: Hash + Eq + CellValue,
    LV: CellValue,
    RK1: Hash + Eq + CellValue,
    RV1: CellValue,
    JK1: Hash + Eq + CellValue,
    MV: CellValue,
    RK2: Hash + Eq + CellValue,
    RV2: CellValue,
    JK2: Hash + Eq + CellValue,
    OV: CellValue,
    FL1: Fn(&LK, &LV) -> JK1 + Send + Sync + 'static,
    FR1: RightJoinKey<RK1, RV1, JK1> + Send + Sync + 'static,
    FM1: Fn(&LK, &LV, &[(RK1, RV1)]) -> MV + Send + Sync + 'static,
    FL2: Fn(&LK, &MV) -> JK2 + Send + Sync + 'static,
    FR2: RightJoinKey<RK2, RV2, JK2> + Send + Sync + 'static,
    FM2: Fn(&LK, &MV, &[(RK2, RV2)]) -> OV + Send + Sync + 'static,
{
    fn new(
        left_key1: FL1,
        right_key1: FR1,
        project1: FM1,
        left_key2: FL2,
        right_key2: FR2,
        project2: FM2,
    ) -> Self {
        Self {
            storage: RuntimeStorage::Serial(TwoKeyedSerial {
                first: JoinState::default(),
                second: JoinState::default(),
            }),
            config: TwoStageConfig {
                left_key1: Arc::new(left_key1),
                right_key1: Arc::new(right_key1),
                project1: Arc::new(project1),
                left_key2: Arc::new(left_key2),
                right_key2: Arc::new(right_key2),
                project2: Arc::new(project2),
            },
            shard_count: query_shard_count(),
        }
    }

    fn propagate_serial(
        serial: &mut TwoKeyedSerial<
            TwoFirstState<LK, LV, RK1, RV1, JK1, MV>,
            TwoSecondState<LK, MV, RK2, RV2, JK2, OV>,
        >,
        config: &TwoStageConfig<FL1, FR1, FM1, FL2, FR2, FM2>,
    ) -> Vec<MapDiff<LK, OV>> {
        let mut scratch1 = std::mem::take(&mut serial.first.scratch);
        scratch1.impacted.move_into(&mut scratch1.impacted_keys);
        let right = serial.first.right.acquire_read();
        let mut scratch2 = std::mem::take(&mut serial.second.scratch);

        for key in &scratch1.impacted_keys {
            let desired = serial.first.left_rows.get(key).map(|left_value| {
                let right_rows = serial
                    .first
                    .left_join_keys
                    .get(key)
                    .and_then(|join_key| right.grouped_rows.get(join_key))
                    .map_or(&[][..], Vec::as_slice);
                (config.project1)(key, left_value, right_rows)
            });

            match (serial.first.output_cache.entry(key.clone()), desired) {
                (Entry::Occupied(mut entry), Some(value)) if entry.get() != &value => {
                    entry.insert(value.clone());
                    upsert_left(
                        &mut serial.second,
                        key.clone(),
                        value,
                        config.left_key2.as_ref(),
                        &mut scratch2.impacted,
                    );
                }
                (Entry::Vacant(entry), Some(value)) => {
                    entry.insert(value.clone());
                    upsert_left(
                        &mut serial.second,
                        key.clone(),
                        value,
                        config.left_key2.as_ref(),
                        &mut scratch2.impacted,
                    );
                }
                (Entry::Occupied(entry), None) => {
                    entry.remove();
                    remove_left(&mut serial.second, key, &mut scratch2.impacted);
                }
                _ => {}
            }
        }
        scratch1.impacted_keys.clear();
        serial.first.scratch = scratch1;

        let output =
            recompute_keyed_impacted(&mut serial.second, &mut scratch2, &|key, value, rights| {
                Some((config.project2)(key, value, rights))
            });
        serial.second.scratch = scratch2;
        output
    }

    fn propagate_atomic_left(
        serial: &mut TwoKeyedSerial<
            TwoFirstState<LK, LV, RK1, RV1, JK1, MV>,
            TwoSecondState<LK, MV, RK2, RV2, JK2, OV>,
        >,
        config: &TwoStageConfig<FL1, FR1, FM1, FL2, FR2, FM2>,
        key: &LK,
        value: &LV,
    ) -> Vec<MapDiff<LK, OV>> {
        serial.first.parallel_active = false;
        serial.second.parallel_active = false;
        let join_key = (config.left_key1)(key, value);
        match serial
            .first
            .left_join_keys
            .insert(key.clone(), join_key.clone())
        {
            Some(previous) if previous != join_key => {
                remove_index_member(&mut serial.first.join_to_left, &previous, key);
                add_index_member(&mut serial.first.join_to_left, join_key, key.clone());
            }
            Some(_) => {}
            None => add_index_member(&mut serial.first.join_to_left, join_key, key.clone()),
        }
        serial.first.left_rows.insert(key.clone(), value.clone());

        let right = serial.first.right.acquire_read();
        let right_rows = serial
            .first
            .left_join_keys
            .get(key)
            .and_then(|join_key| right.grouped_rows.get(join_key))
            .map_or(&[][..], Vec::as_slice);
        let middle = (config.project1)(key, value, right_rows);
        if serial.first.output_cache.get(key) == Some(&middle) {
            return Vec::new();
        }
        serial
            .first
            .output_cache
            .insert(key.clone(), middle.clone());

        let join_key = (config.left_key2)(key, &middle);
        match serial
            .second
            .left_join_keys
            .insert(key.clone(), join_key.clone())
        {
            Some(previous) if previous != join_key => {
                remove_index_member(&mut serial.second.join_to_left, &previous, key);
                add_index_member(&mut serial.second.join_to_left, join_key, key.clone());
            }
            Some(_) => {}
            None => add_index_member(&mut serial.second.join_to_left, join_key, key.clone()),
        }
        serial.second.left_rows.insert(key.clone(), middle.clone());

        let right = serial.second.right.acquire_read();
        let right_rows = serial
            .second
            .left_join_keys
            .get(key)
            .and_then(|join_key| right.grouped_rows.get(join_key))
            .map_or(&[][..], Vec::as_slice);
        let final_value = (config.project2)(key, &middle, right_rows);
        let mut output = Vec::with_capacity(1);
        commit_keyed_value(
            &mut serial.second.output_cache,
            key.clone(),
            Some(final_value),
            &mut output,
        );
        output
    }

    fn propagate_sharded(
        sharded: &mut TwoKeyedSharded<
            TwoFirstShards<LK, LV, RK1, RV1, JK1, MV>,
            TwoSecondShards<LK, MV, RK2, RV2, JK2, OV>,
        >,
        config: &TwoStageConfig<FL1, FR1, FM1, FL2, FR2, FM2>,
        first_shard_changes: Vec<Vec<MapDiff<LK, MV>>>,
        first_event_ordinals: &FxHashMap<LK, u64>,
    ) -> Vec<MapDiff<LK, OV>> {
        let intermediate = sharded
            .first
            .merge_changes(first_shard_changes, first_event_ordinals);
        let (routed_second, second_event_ordinals) = sharded
            .second
            .route_left_owned(intermediate, config.left_key2.as_ref());
        let second_shard_changes = process_left_shards(
            &mut sharded.second,
            &routed_second,
            config.left_key2.as_ref(),
            config.project2.as_ref(),
        );
        sharded
            .second
            .merge_changes(second_shard_changes, &second_event_ordinals)
    }

    fn build_sharded(
        serial: &TwoKeyedSerial<
            TwoFirstState<LK, LV, RK1, RV1, JK1, MV>,
            TwoSecondState<LK, MV, RK2, RV2, JK2, OV>,
        >,
        config: &TwoStageConfig<FL1, FR1, FM1, FL2, FR2, FM2>,
        shard_count: usize,
    ) -> TwoKeyedSharded<
        TwoFirstShards<LK, LV, RK1, RV1, JK1, MV>,
        TwoSecondShards<LK, MV, RK2, RV2, JK2, OV>,
    > {
        let mut first = ShardedKeyedJoin::new(shard_count);
        let first_left = MapDiff::Initial {
            entries: state_left_entries(&serial.first),
        };
        let (routed, _) = first.route_left(&first_left, config.left_key1.as_ref());
        let _ = process_left_shards(
            &mut first,
            &routed,
            config.left_key1.as_ref(),
            config.project1.as_ref(),
        );
        let first_right = MapDiff::Initial {
            entries: state_right_entries(&serial.first),
        };
        let routed = first.route_right(&first_right, config.right_key1.as_ref());
        let _ = process_right_shards(
            &mut first,
            &routed,
            config.right_key1.as_ref(),
            config.project1.as_ref(),
        );

        let mut second = ShardedKeyedJoin::new(shard_count);
        let second_left = MapDiff::Initial {
            entries: state_left_entries(&serial.second),
        };
        let (routed, _) = second.route_left(&second_left, config.left_key2.as_ref());
        let _ = process_left_shards(
            &mut second,
            &routed,
            config.left_key2.as_ref(),
            config.project2.as_ref(),
        );
        let second_right = MapDiff::Initial {
            entries: state_right_entries(&serial.second),
        };
        let routed = second.route_right(&second_right, config.right_key2.as_ref());
        let _ = process_right_shards(
            &mut second,
            &routed,
            config.right_key2.as_ref(),
            config.project2.as_ref(),
        );
        TwoKeyedSharded { first, second }
    }

    fn promote_for(&mut self, work: usize) {
        if self.storage.is_serial() && self.shard_count > 1 && work >= 8_192 {
            let config = &self.config;
            let shard_count = self.shard_count;
            self.storage
                .promote_with(|serial| Self::build_sharded(serial, config, shard_count));
        }
    }

    fn apply_left_diff(&mut self, diff: &MapDiff<LK, LV>) -> BatchedChanges<LK, OV> {
        self.promote_for(diff.work_items());
        let config = &self.config;
        let changes = match &mut self.storage {
            RuntimeStorage::Serial(serial) => match diff {
                MapDiff::Insert { key, value }
                | MapDiff::Update {
                    key,
                    new_value: value,
                    ..
                } => Self::propagate_atomic_left(serial, config, key, value),
                MapDiff::Initial { .. } | MapDiff::Remove { .. } | MapDiff::Batch { .. } => {
                    let mut scratch = std::mem::take(&mut serial.first.scratch);
                    apply_left_diff(
                        &mut serial.first,
                        diff,
                        config.left_key1.as_ref(),
                        &mut scratch.impacted,
                    );
                    serial.first.scratch = scratch;
                    Self::propagate_serial(serial, config)
                }
            },
            RuntimeStorage::Sharded {
                runtime: sharded, ..
            } => {
                let (routed, event_ordinals) =
                    sharded.first.route_left(diff, config.left_key1.as_ref());
                let shard_changes = process_left_shards(
                    &mut sharded.first,
                    &routed,
                    config.left_key1.as_ref(),
                    config.project1.as_ref(),
                );
                Self::propagate_sharded(sharded, config, shard_changes, &event_ordinals)
            }
        };
        BatchedChanges(changes)
    }

    fn apply_first_right_diff(&mut self, diff: &MapDiff<RK1, RV1>) -> BatchedChanges<LK, OV> {
        self.promote_for(diff.work_items());
        let config = &self.config;
        let changes = match &mut self.storage {
            RuntimeStorage::Serial(serial) => {
                let mut scratch = std::mem::take(&mut serial.first.scratch);
                apply_right_diff(
                    &mut serial.first,
                    diff,
                    config.right_key1.as_ref(),
                    &mut scratch.impacted,
                    &mut scratch.changed_join_keys,
                );
                serial.first.scratch = scratch;
                Self::propagate_serial(serial, config)
            }
            RuntimeStorage::Sharded {
                runtime: sharded, ..
            } => {
                let routed = sharded.first.route_right(diff, config.right_key1.as_ref());
                let shard_changes = process_right_shards(
                    &mut sharded.first,
                    &routed,
                    config.right_key1.as_ref(),
                    config.project1.as_ref(),
                );
                Self::propagate_sharded(sharded, config, shard_changes, &FxHashMap::default())
            }
        };
        BatchedChanges(changes)
    }

    fn apply_second_right_diff(&mut self, diff: &MapDiff<RK2, RV2>) -> BatchedChanges<LK, OV> {
        self.promote_for(diff.work_items());
        let config = &self.config;
        let changes = match &mut self.storage {
            RuntimeStorage::Serial(serial) => {
                let mut scratch = std::mem::take(&mut serial.second.scratch);
                apply_right_diff(
                    &mut serial.second,
                    diff,
                    config.right_key2.as_ref(),
                    &mut scratch.impacted,
                    &mut scratch.changed_join_keys,
                );
                let changes = recompute_keyed_impacted(
                    &mut serial.second,
                    &mut scratch,
                    &|key, value, rights| Some((config.project2)(key, value, rights)),
                );
                serial.second.scratch = scratch;
                changes
            }
            RuntimeStorage::Sharded {
                runtime: sharded, ..
            } => {
                let routed = sharded.second.route_right(diff, config.right_key2.as_ref());
                let shard_changes = process_right_shards(
                    &mut sharded.second,
                    &routed,
                    config.right_key2.as_ref(),
                    config.project2.as_ref(),
                );
                sharded
                    .second
                    .merge_changes(shard_changes, &FxHashMap::default())
            }
        };
        BatchedChanges(changes)
    }
}

#[allow(clippy::type_complexity)]
impl<LK, LV, RK1, RV1, JK1, MV, RK2, RV2, JK2, OV, FL1, FR1, FM1, FL2, FR2, FM2, First, Second>
    InstallRegionRights<
        TwoStageKernel<LK, LV, RK1, RV1, JK1, MV, RK2, RV2, JK2, OV, FL1, FR1, FM1, FL2, FR2, FM2>,
        LK,
        OV,
        LegacyTransaction,
    > for TwoStageRights<First, Second>
where
    LK: Hash + Eq + CellValue,
    LV: CellValue,
    RK1: Hash + Eq + CellValue,
    RV1: CellValue,
    JK1: Hash + Eq + CellValue,
    MV: CellValue,
    RK2: Hash + Eq + CellValue,
    RV2: CellValue,
    JK2: Hash + Eq + CellValue,
    OV: CellValue,
    FL1: Fn(&LK, &LV) -> JK1 + Send + Sync + 'static,
    FR1: RightJoinKey<RK1, RV1, JK1> + Send + Sync + 'static,
    FM1: Fn(&LK, &LV, &[(RK1, RV1)]) -> MV + Send + Sync + 'static,
    FL2: Fn(&LK, &MV) -> JK2 + Send + Sync + 'static,
    FR2: RightJoinKey<RK2, RV2, JK2> + Send + Sync + 'static,
    FM2: Fn(&LK, &MV, &[(RK2, RV2)]) -> OV + Send + Sync + 'static,
    First: crate::map_query::MapQuery<Key = RK1, Value = RV1>,
    Second: crate::map_query::MapQuery<Key = RK2, Value = RV2>,
{
    fn install(
        self,
        cx: &mut crate::map_query::compiler::CompileContext,
        host: &Arc<
            RegionHost<
                TwoStageKernel<
                    LK,
                    LV,
                    RK1,
                    RV1,
                    JK1,
                    MV,
                    RK2,
                    RV2,
                    JK2,
                    OV,
                    FL1,
                    FR1,
                    FM1,
                    FL2,
                    FR2,
                    FM2,
                >,
                LK,
                OV,
                LegacyTransaction,
            >,
        >,
    ) -> Vec<SubscriptionGuard> {
        let first_host = Arc::clone(host);
        let mut guards = crate::map_query::compile_runtime_into(
            self.first,
            cx,
            Arc::new(move |diff: &MapDiff<RK1, RV1>| {
                first_host.dispatch(|kernel| kernel.apply_first_right_diff(diff));
            }),
        );

        let second_host = Arc::clone(host);
        guards.extend(crate::map_query::compile_runtime_into(
            self.second,
            cx,
            Arc::new(move |diff: &MapDiff<RK2, RV2>| {
                second_host.dispatch(|kernel| kernel.apply_second_right_diff(diff));
            }),
        ));
        guards
    }
}

/// Install two consecutive left-key-preserving joins as one coordinated
/// runtime. All three roots enter one state lock directly; intermediate rows
/// are carried from the first join into the second without a subscriber or
/// dynamically dispatched callback boundary.
#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    clippy::type_complexity
)]
pub fn install_two_keyed_join_runtime_via_query<
    LK,
    LV,
    RK1,
    RV1,
    JK1,
    MV,
    RK2,
    RV2,
    JK2,
    OV,
    L,
    R1,
    R2,
    FL1,
    FR1,
    FM1,
    FL2,
    FR2,
    FM2,
>(
    cx: &mut crate::map_query::compiler::CompileContext,
    left: L,
    right1: R1,
    right2: R2,
    left_join_key1: FL1,
    right_join_key1: FR1,
    map_first: FM1,
    left_join_key2: FL2,
    right_join_key2: FR2,
    map_second: FM2,
    sink: crate::map_query::BoxedMapDiffSink<LK, OV>,
) -> Vec<SubscriptionGuard>
where
    LK: Hash + Eq + CellValue,
    LV: CellValue,
    RK1: Hash + Eq + CellValue,
    RV1: CellValue,
    JK1: Hash + Eq + CellValue,
    MV: CellValue,
    RK2: Hash + Eq + CellValue,
    RV2: CellValue,
    JK2: Hash + Eq + CellValue,
    OV: CellValue,
    L: crate::map_query::MapQuery<Key = LK, Value = LV>,
    R1: crate::map_query::MapQuery<Key = RK1, Value = RV1>,
    R2: crate::map_query::MapQuery<Key = RK2, Value = RV2>,
    FL1: Fn(&LK, &LV) -> JK1 + Send + Sync + 'static,
    FR1: RightJoinKey<RK1, RV1, JK1> + Send + Sync + 'static,
    FM1: Fn(&LK, &LV, &[(RK1, RV1)]) -> MV + Send + Sync + 'static,
    FL2: Fn(&LK, &MV) -> JK2 + Send + Sync + 'static,
    FR2: RightJoinKey<RK2, RV2, JK2> + Send + Sync + 'static,
    FM2: Fn(&LK, &MV, &[(RK2, RV2)]) -> OV + Send + Sync + 'static,
{
    let kernel = TwoStageKernel::new(
        left_join_key1,
        right_join_key1,
        map_first,
        left_join_key2,
        right_join_key2,
        map_second,
    );
    let right_roots = TwoStageRights {
        first: right1,
        second: right2,
    };
    install_region_runtime(
        cx,
        left,
        right_roots,
        kernel,
        RootRegistrationOrder::LeftThenRights,
        LegacyTransaction,
        sink,
        TwoStageKernel::apply_left_diff,
    )
}
