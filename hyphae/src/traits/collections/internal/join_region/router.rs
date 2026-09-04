use std::{
    hash::{Hash, Hasher},
    marker::PhantomData,
};

use rustc_hash::{FxHashMap, FxHasher};

use crate::{cell_map::MapDiff, traits::CellValue};

use super::{
    super::join_lifecycle::RuntimeStorage,
    declaration::JCons,
    stage_runtime::{
        EmptyShardRuntime, ExecutableStage, HeadInputSnapshot, RuntimeStageCost, RuntimeStages,
        batch_has_unique_atomic_keys,
    },
};

/// Dual-mode whole-region executor. Tiny events use the original single
/// runtime without extra hashing. Promotion is one-way; afterwards every map
/// key owns one persistent shard for the entire heterogeneous stage spine.
pub(super) struct RegionRouter<Runtime, K, Input> {
    storage: RuntimeStorage<Runtime, Vec<Runtime>>,
    key_sequence: FxHashMap<K, u64>,
    next_sequence: u64,
    shard_count: usize,
    promotion_work: usize,
    _input: PhantomData<fn() -> Input>,
}

const DEFAULT_PROMOTION_WORK: usize = 8_192;
// The frozen four-stage workload clears the strict 1.5x confidence-bound gate
// at the first 200k-cost batch. The wide band also prevents oscillation.
const PARALLEL_REGION_WORK_ENTER: usize = 200_000;
const PARALLEL_REGION_WORK_EXIT: usize = 96_000;

#[allow(clippy::missing_const_for_fn)]
fn configured_shards() -> usize {
    #[cfg(feature = "scheduler")]
    let count = crate::executor::configured_worker_threads().max(1);
    #[cfg(not(feature = "scheduler"))]
    let count = 1;
    count
}

const fn configured_promotion_work() -> usize {
    DEFAULT_PROMOTION_WORK
}

#[allow(
    clippy::arithmetic_side_effects,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::items_after_statements,
    clippy::too_many_lines
)]
impl<Runtime, K, Input> RegionRouter<Runtime, K, Input>
where
    K: Hash + Eq + CellValue,
    Input: CellValue,
    Runtime: RuntimeStages<K, Input>
        + EmptyShardRuntime
        + HeadInputSnapshot<K, Input>
        + RuntimeStageCost
        + Send,
{
    pub(super) fn new(runtime: Runtime) -> Self {
        Self {
            storage: RuntimeStorage::Serial(runtime),
            key_sequence: FxHashMap::default(),
            next_sequence: 0,
            shard_count: configured_shards(),
            promotion_work: configured_promotion_work(),
            _input: PhantomData,
        }
    }

    fn shard_for(key: &K, count: usize) -> usize {
        let mut hasher = FxHasher::default();
        key.hash(&mut hasher);
        let count = u64::try_from(count.max(1)).unwrap_or(1);
        let index = hasher.finish().checked_rem(count).unwrap_or(0);
        usize::try_from(index).unwrap_or(0)
    }

    fn remember(&mut self, diff: &MapDiff<K, Input>) {
        match diff {
            MapDiff::Initial { entries } => {
                self.key_sequence.clear();
                self.next_sequence = 0;
                for (key, _) in entries {
                    self.key_sequence.insert(key.clone(), self.next_sequence);
                    self.next_sequence += 1;
                }
            }
            MapDiff::Insert { key, .. } | MapDiff::Update { key, .. } => {
                if !self.key_sequence.contains_key(key) {
                    self.key_sequence.insert(key.clone(), self.next_sequence);
                    self.next_sequence += 1;
                }
            }
            MapDiff::Remove { key, .. } => {
                self.key_sequence.remove(key);
            }
            MapDiff::Batch { changes } => changes.iter().for_each(|change| self.remember(change)),
        }
    }

    fn merge_order(&self, diff: &MapDiff<K, Input>) -> FxHashMap<K, u64> {
        let mut order = FxHashMap::default();
        let mut next = 0;
        fn remember<K: Hash + Eq + Clone>(key: &K, order: &mut FxHashMap<K, u64>, next: &mut u64) {
            if !order.contains_key(key) {
                order.insert(key.clone(), *next);
                *next = next.saturating_add(1);
            }
        }
        fn visit<K: Hash + Eq + Clone, V>(
            diff: &MapDiff<K, V>,
            existing: &mut FxHashMap<K, u64>,
            next_sequence: &mut u64,
            order: &mut FxHashMap<K, u64>,
            next: &mut u64,
        ) {
            match diff {
                MapDiff::Initial { entries } => {
                    // An Initial has its own deterministic event-local order:
                    // the live rows in saved sequence, followed by new rows in
                    // entry order. Overwrite earlier tiebreaks because tagged
                    // output ordinals keep separate input events apart.
                    let mut old: Vec<_> = existing.iter().collect();
                    old.sort_by_key(|(_, sequence)| **sequence);
                    let mut initial_next = 0;
                    for (key, _) in old {
                        order.insert(key.clone(), initial_next);
                        initial_next = initial_next.saturating_add(1);
                    }
                    for (key, _) in entries {
                        if !order.contains_key(key) {
                            order.insert(key.clone(), initial_next);
                            initial_next = initial_next.saturating_add(1);
                        }
                    }
                    existing.clear();
                    *next_sequence = 0;
                    for (key, _) in entries {
                        existing.insert(key.clone(), *next_sequence);
                        *next_sequence = next_sequence.saturating_add(1);
                    }
                }
                MapDiff::Insert { key, .. } | MapDiff::Update { key, .. } => {
                    remember(key, order, next);
                    if !existing.contains_key(key) {
                        existing.insert(key.clone(), *next_sequence);
                        *next_sequence = next_sequence.saturating_add(1);
                    }
                }
                MapDiff::Remove { key, .. } => {
                    remember(key, order, next);
                    existing.remove(key);
                }
                MapDiff::Batch { changes } => changes.iter().for_each(|change| {
                    visit(change, existing, next_sequence, order, next);
                }),
            }
        }
        let mut existing = self.key_sequence.clone();
        let mut next_sequence = existing
            .values()
            .copied()
            .max()
            .map_or(0, |value| value.saturating_add(1));
        visit(
            diff,
            &mut existing,
            &mut next_sequence,
            &mut order,
            &mut next,
        );
        order
    }

    fn promote(&mut self) {
        let count = self.shard_count.max(1);
        let key_sequence = &self.key_sequence;
        self.storage.promote_with(|sequential| {
            let mut shards: Vec<_> = (0..count).map(|_| sequential.empty_shard()).collect();
            // Replay in stable source order. Outputs are intentionally discarded.
            let mut keys: Vec<_> = key_sequence.iter().collect();
            keys.sort_by_key(|(_, sequence)| **sequence);
            for (key, _) in keys {
                if let Some(value) = sequential.head_input(key) {
                    let shard = Self::shard_for(key, count);
                    let _ = shards[shard].apply_left_diff(&MapDiff::Insert {
                        key: key.clone(),
                        value,
                    });
                }
            }
            shards
        });
    }

    fn order_changes<Output: CellValue>(
        order: &FxHashMap<K, u64>,
        changes: &mut [MapDiff<K, Output>],
    ) {
        changes.sort_by_key(|change| {
            change
                .atomic_key()
                .and_then(|key| order.get(key))
                .copied()
                .unwrap_or(u64::MAX)
        });
    }

    fn route_diff(
        diff: &MapDiff<K, Input>,
        shard_count: usize,
        next_ordinal: &mut u64,
        routed: &mut [Vec<(u64, MapDiff<K, Input>)>],
    ) {
        match diff {
            MapDiff::Batch { changes } => {
                for change in changes {
                    Self::route_diff(change, shard_count, next_ordinal, routed);
                }
            }
            MapDiff::Initial { entries } => {
                let ordinal = *next_ordinal;
                *next_ordinal = next_ordinal.saturating_add(1);
                let mut partitioned = vec![Vec::new(); shard_count];
                for (key, value) in entries {
                    partitioned[Self::shard_for(key, shard_count)]
                        .push((key.clone(), value.clone()));
                }
                for (id, entries) in partitioned.into_iter().enumerate() {
                    routed[id].push((ordinal, MapDiff::Initial { entries }));
                }
            }
            other => {
                let ordinal = *next_ordinal;
                *next_ordinal = next_ordinal.saturating_add(1);
                let key = other.atomic_key().expect("non-container diff has a key");
                routed[Self::shard_for(key, shard_count)].push((ordinal, other.clone()));
            }
        }
    }

    fn event_orders(&self, diff: &MapDiff<K, Input>) -> Vec<FxHashMap<K, u64>> {
        fn visit<K: Hash + Eq + Clone, V>(
            diff: &MapDiff<K, V>,
            existing: &mut FxHashMap<K, u64>,
            next_sequence: &mut u64,
            orders: &mut Vec<FxHashMap<K, u64>>,
        ) {
            if let MapDiff::Batch { changes } = diff {
                for change in changes {
                    visit(change, existing, next_sequence, orders);
                }
                return;
            }
            let mut event = FxHashMap::default();
            match diff {
                MapDiff::Initial { entries } => {
                    let mut old: Vec<_> = existing.iter().collect();
                    old.sort_by_key(|(_, sequence)| **sequence);
                    let mut rank: u64 = 0;
                    for (key, _) in old {
                        event.insert(key.clone(), rank);
                        rank = rank.saturating_add(1);
                    }
                    for (key, _) in entries {
                        if !event.contains_key(key) {
                            event.insert(key.clone(), rank);
                            rank = rank.saturating_add(1);
                        }
                    }
                    existing.clear();
                    *next_sequence = 0;
                    for (key, _) in entries {
                        existing.insert(key.clone(), *next_sequence);
                        *next_sequence = next_sequence.saturating_add(1);
                    }
                }
                MapDiff::Insert { key, .. } | MapDiff::Update { key, .. } => {
                    event.insert(key.clone(), 0);
                    if !existing.contains_key(key) {
                        existing.insert(key.clone(), *next_sequence);
                        *next_sequence = next_sequence.saturating_add(1);
                    }
                }
                MapDiff::Remove { key, .. } => {
                    event.insert(key.clone(), 0);
                    existing.remove(key);
                }
                MapDiff::Batch { .. } => return,
            }
            orders.push(event);
        }
        let mut existing = self.key_sequence.clone();
        let mut next_sequence = existing
            .values()
            .copied()
            .max()
            .map_or(0, |value| value.saturating_add(1));
        let mut orders = Vec::new();
        visit(diff, &mut existing, &mut next_sequence, &mut orders);
        orders
    }

    fn apply_left_eventwise(
        &mut self,
        diff: &MapDiff<K, Input>,
        output: &mut Vec<MapDiff<K, Runtime::Output>>,
    ) -> bool {
        if let MapDiff::Batch { changes } = diff {
            for change in changes {
                if !self.apply_left_eventwise(change, output) {
                    return false;
                }
            }
            return true;
        }
        let order = self.merge_order(diff);
        let changes = match &mut self.storage {
            RuntimeStorage::Serial(runtime) => runtime.apply_left_diff(diff),
            RuntimeStorage::Sharded { .. } => return false,
        };
        let mut flat = Vec::new();
        for change in changes {
            change.flatten_into(&mut flat);
        }
        Self::order_changes(&order, &mut flat);
        output.extend(flat);
        self.remember(diff);
        true
    }

    fn apply_serial_left(
        &mut self,
        diff: &MapDiff<K, Input>,
        eventwise: bool,
    ) -> Option<Vec<MapDiff<K, Runtime::Output>>> {
        if !self.storage.is_serial() {
            return None;
        }
        #[cfg(feature = "region-calibration")]
        crate::region_calibration::left_serial_dispatch();
        if eventwise {
            let mut changes = Vec::new();
            if !self.apply_left_eventwise(diff, &mut changes) {
                return None;
            }
            return Some(vec![MapDiff::Batch { changes }]);
        }
        if matches!(diff, MapDiff::Initial { .. }) {
            let order = self.merge_order(diff);
            let mut output = match &mut self.storage {
                RuntimeStorage::Serial(runtime) => runtime.apply_left_diff(diff),
                RuntimeStorage::Sharded { .. } => return None,
            };
            Self::order_changes(&order, &mut output);
            self.remember(diff);
            return Some(output);
        }
        self.remember(diff);
        match &mut self.storage {
            RuntimeStorage::Serial(runtime) => Some(runtime.apply_left_diff(diff)),
            RuntimeStorage::Sharded { .. } => None,
        }
    }

    pub(super) fn apply_left(
        &mut self,
        diff: &MapDiff<K, Input>,
    ) -> Vec<MapDiff<K, Runtime::Output>> {
        let batch_is_unique = match diff {
            MapDiff::Batch { changes } => Some(batch_has_unique_atomic_keys(changes)),
            _ => None,
        };
        let non_unique_batch = matches!(batch_is_unique, Some(false));
        let is_serial = self.storage.is_serial();
        if is_serial
            && self.shard_count <= 1
            && let Some(output) = self.apply_serial_left(diff, non_unique_batch)
        {
            return output;
        }
        let estimated_work = diff.work_items().saturating_mul(Runtime::COST_UNITS);
        let promotion_warranted = diff.work_items() >= self.promotion_work
            || estimated_work >= PARALLEL_REGION_WORK_ENTER;
        if is_serial
            && ((self.shard_count <= 1) || !promotion_warranted)
            && let Some(output) = self.apply_serial_left(diff, non_unique_batch)
        {
            return output;
        }
        if is_serial {
            self.promote();
        }

        let order = self.merge_order(diff);
        let event_orders = non_unique_batch.then(|| self.event_orders(diff));
        let preserve_batch = batch_is_unique.is_some();
        let unique_batch = batch_is_unique.unwrap_or(false);

        let RuntimeStorage::Sharded {
            runtime: shards,
            parallel_active,
        } = &mut self.storage
        else {
            return self
                .apply_serial_left(diff, non_unique_batch)
                .unwrap_or_default();
        };
        let mut routed = vec![Vec::new(); shards.len()];
        let mut next_ordinal = 0;
        Self::route_diff(diff, shards.len(), &mut next_ordinal, &mut routed);

        let hysteresis_wants_parallel = if *parallel_active {
            estimated_work >= PARALLEL_REGION_WORK_EXIT
        } else {
            estimated_work >= PARALLEL_REGION_WORK_ENTER
        };
        let shard_work: Vec<_> = routed
            .iter()
            .map(|changes| {
                changes.iter().fold(0_usize, |work, (_, change)| {
                    work.saturating_add(change.work_items())
                })
            })
            .collect();
        let active_shards = shard_work.iter().filter(|work| **work != 0).count();
        let max_shard_work = shard_work.iter().copied().max().unwrap_or(0);
        let balanced = active_shards > 1
            && max_shard_work.saturating_mul(4) <= diff.work_items().saturating_mul(3);
        #[cfg(not(all(feature = "scheduler", not(target_arch = "wasm32"))))]
        let _ = (hysteresis_wants_parallel, balanced);
        #[cfg(all(feature = "scheduler", not(target_arch = "wasm32")))]
        let resources_available =
            hysteresis_wants_parallel && balanced && crate::executor::worker_pool().is_some();
        #[cfg(not(all(feature = "scheduler", not(target_arch = "wasm32"))))]
        let resources_available = false;
        #[cfg(feature = "region-calibration")]
        let was_parallel = *parallel_active;
        *parallel_active = resources_available;
        #[cfg(feature = "region-calibration")]
        match (was_parallel, *parallel_active) {
            (false, true) => crate::region_calibration::inactive_to_parallel(),
            (true, false) => crate::region_calibration::parallel_to_inactive(),
            _ => {}
        }
        let run_parallel = *parallel_active;

        let process = |(shard_id, (shard, changes)): (
            usize,
            (&mut Runtime, Vec<(u64, MapDiff<K, Input>)>),
        )| {
            let mut tagged = Vec::new();
            if unique_batch && !changes.is_empty() {
                let batch = MapDiff::Batch {
                    changes: changes.into_iter().map(|(_, change)| change).collect(),
                };
                let mut flat = Vec::new();
                for change in shard.apply_left_diff(&batch) {
                    change.flatten_into(&mut flat);
                }
                for (local, change) in flat.into_iter().enumerate() {
                    let ordinal = change
                        .atomic_key()
                        .and_then(|key| order.get(key))
                        .copied()
                        .unwrap_or(u64::MAX);
                    tagged.push((ordinal, local, shard_id, change));
                }
            } else {
                for (ordinal, change) in changes {
                    let mut flat = Vec::new();
                    for change in shard.apply_left_diff(&change) {
                        change.flatten_into(&mut flat);
                    }
                    for (local, output) in flat.into_iter().enumerate() {
                        tagged.push((ordinal, local, shard_id, output));
                    }
                }
            }
            tagged
        };

        #[cfg(all(feature = "scheduler", not(target_arch = "wasm32")))]
        let per_shard = if run_parallel {
            if let Some(pool) = crate::executor::worker_pool() {
                #[cfg(feature = "region-calibration")]
                crate::region_calibration::left_parallel_dispatch();
                use rayon::prelude::*;
                pool.install(|| {
                    shards
                        .par_iter_mut()
                        .zip(routed.into_par_iter())
                        .enumerate()
                        .map(process)
                        .collect::<Vec<_>>()
                })
            } else {
                #[cfg(feature = "region-calibration")]
                crate::region_calibration::left_serial_dispatch();
                shards
                    .iter_mut()
                    .zip(routed)
                    .enumerate()
                    .map(process)
                    .collect()
            }
        } else {
            #[cfg(feature = "region-calibration")]
            crate::region_calibration::left_serial_dispatch();
            shards
                .iter_mut()
                .zip(routed)
                .enumerate()
                .map(process)
                .collect()
        };
        #[cfg(not(all(feature = "scheduler", not(target_arch = "wasm32"))))]
        let per_shard: Vec<_> = {
            let _ = run_parallel;
            #[cfg(feature = "region-calibration")]
            crate::region_calibration::left_serial_dispatch();
            shards
                .iter_mut()
                .zip(routed)
                .enumerate()
                .map(process)
                .collect()
        };

        let mut tagged: Vec<_> = per_shard.into_iter().flatten().collect();
        tagged.sort_by_key(|(ordinal, local, shard, change)| {
            let key_order = if unique_batch || event_orders.is_none() {
                change
                    .atomic_key()
                    .and_then(|key| order.get(key))
                    .copied()
                    .unwrap_or(u64::MAX)
            } else {
                usize::try_from(*ordinal)
                    .ok()
                    .and_then(|index| event_orders.as_ref().and_then(|orders| orders.get(index)))
                    .and_then(|event| change.atomic_key().and_then(|key| event.get(key)))
                    .copied()
                    .unwrap_or(u64::MAX)
            };
            (*ordinal, key_order, *local, *shard)
        });
        let output = tagged.into_iter().map(|(_, _, _, change)| change).collect();
        self.remember(diff);
        if preserve_batch {
            vec![MapDiff::Batch { changes: output }]
        } else {
            output
        }
    }

    fn apply_serial_right<Location, RK, RV>(
        runtime: &mut Runtime,
        order: &FxHashMap<K, u64>,
        diff: &MapDiff<RK, RV>,
        maintain: bool,
    ) -> Vec<MapDiff<K, Runtime::Output>>
    where
        RK: Hash + Eq + CellValue,
        RV: CellValue,
        Runtime: RightRoot<Location, K, Input, RK, RV>,
    {
        if !matches!(diff, MapDiff::Batch { .. }) {
            let mut output = runtime.apply_right_root_diff_policy(diff, maintain);
            Self::order_changes(order, &mut output);
            return output;
        }

        let mut leaves = Vec::new();
        diff.visit_leaves(&mut |leaf| leaves.push(leaf));
        let mut output = Vec::new();
        for leaf in leaves {
            let mut phase = Vec::new();
            for change in runtime.apply_right_root_diff_policy(leaf, maintain) {
                change.flatten_into(&mut phase);
            }
            Self::order_changes(order, &mut phase);
            output.extend(phase);
        }
        vec![MapDiff::Batch { changes: output }]
    }

    pub(super) fn apply_right<Location, RK, RV>(
        &mut self,
        diff: &MapDiff<RK, RV>,
        maintain: bool,
    ) -> Vec<MapDiff<K, Runtime::Output>>
    where
        RK: Hash + Eq + CellValue,
        RV: CellValue,
        Runtime: RightRoot<Location, K, Input, RK, RV>,
    {
        // Canonical router traces follow stable left-source order in every
        // execution mode. Raw stage-kernel bucket order is a hash/index
        // implementation detail and cannot be reconstructed across shards.
        let is_serial = self.storage.is_serial();
        if self.shard_count <= 1
            && let RuntimeStorage::Serial(runtime) = &mut self.storage
        {
            return Self::apply_serial_right::<Location, RK, RV>(
                runtime,
                &self.key_sequence,
                diff,
                maintain,
            );
        }
        if is_serial && self.shard_count > 1 && diff.work_items() >= self.promotion_work {
            self.promote();
        }
        match &mut self.storage {
            RuntimeStorage::Sharded {
                runtime: shards, ..
            } => {
                let order = &self.key_sequence;
                let preserve_batch = matches!(diff, MapDiff::Batch { .. });
                let mut leaves = Vec::new();
                diff.visit_leaves(&mut |leaf| leaves.push(leaf));
                let mut output = Vec::new();
                // Advance the shared physical index one source member at a time.
                // Every observer shard therefore reads the same snapshot that the
                // sequential runtime used for this phase before the next member.
                for leaf in leaves {
                    let mut phase = Vec::new();
                    for (id, shard) in shards.iter_mut().enumerate() {
                        for change in shard.apply_right_root_diff_policy(leaf, maintain && id == 0)
                        {
                            change.flatten_into(&mut phase);
                        }
                    }
                    Self::order_changes(order, &mut phase);
                    output.extend(phase);
                }
                if preserve_batch {
                    vec![MapDiff::Batch { changes: output }]
                } else {
                    output
                }
            }
            RuntimeStorage::Serial(runtime) => Self::apply_serial_right::<Location, RK, RV>(
                runtime,
                &self.key_sequence,
                diff,
                maintain,
            ),
        }
    }
}

/// Select the first right root in a runtime-stage list.
pub(super) struct Here;

/// Select a right root in the tail; nesting counts stages from the front.
pub(super) struct There<Location>(PhantomData<fn() -> Location>);

/// Direct entry from one right root into its selected stage.
///
/// `Here` updates the head and propagates its output through the tail. A
/// `There<L>` implementation delegates directly to the tail, so earlier stages
/// are not re-executed when a later right root changes.
pub(super) trait RightRoot<Location, K, Input, RK, RV>: RuntimeStages<K, Input>
where
    K: Hash + Eq + CellValue,
    Input: CellValue,
    RK: Hash + Eq + CellValue,
    RV: CellValue,
{
    fn apply_right_root_diff(&mut self, diff: &MapDiff<RK, RV>) -> Vec<MapDiff<K, Self::Output>> {
        self.apply_right_root_diff_policy(diff, true)
    }

    fn apply_right_root_diff_policy(
        &mut self,
        diff: &MapDiff<RK, RV>,
        maintain: bool,
    ) -> Vec<MapDiff<K, Self::Output>>;
}

impl<K, Input, RK, RV, Head, Tail> RightRoot<Here, K, Input, RK, RV> for JCons<Head, Tail>
where
    K: Hash + Eq + CellValue,
    Input: CellValue,
    RK: Hash + Eq + CellValue,
    RV: CellValue,
    Head: ExecutableStage<Key = K, Input = Input, RightKey = RK, RightValue = RV>,
    Tail: RuntimeStages<K, Head::Output>,
{
    fn apply_right_root_diff_policy(
        &mut self,
        diff: &MapDiff<RK, RV>,
        maintain: bool,
    ) -> Vec<MapDiff<K, Self::Output>> {
        let preserve_batch = matches!(diff, MapDiff::Batch { .. });
        let head_changes = self.head.apply_right_diff(diff, maintain);
        let mut output = Vec::new();
        for change in &head_changes {
            output.extend(self.tail.apply_left_diff(change));
        }
        if preserve_batch {
            vec![MapDiff::Batch { changes: output }]
        } else {
            output
        }
    }
}

impl<Location, K, Input, RK, RV, Head, Tail> RightRoot<There<Location>, K, Input, RK, RV>
    for JCons<Head, Tail>
where
    K: Hash + Eq + CellValue,
    Input: CellValue,
    RK: Hash + Eq + CellValue,
    RV: CellValue,
    Head: ExecutableStage<Key = K, Input = Input>,
    Tail: RuntimeStages<K, Head::Output> + RightRoot<Location, K, Head::Output, RK, RV>,
{
    fn apply_right_root_diff_policy(
        &mut self,
        diff: &MapDiff<RK, RV>,
        maintain: bool,
    ) -> Vec<MapDiff<K, Self::Output>> {
        self.tail.apply_right_root_diff_policy(diff, maintain)
    }
}
