mod sharded;
mod state;
mod two_stage;

pub use two_stage::install_two_keyed_join_runtime_via_query;

use std::{
    any::TypeId,
    hash::Hash,
    sync::{Arc, Mutex},
};

use crate::{
    cell_map::MapDiff,
    subscription::SubscriptionGuard,
    traits::{CellValue, RightJoinKey},
};

use state::{
    JoinState, apply_left_diff, apply_right_diff, observe_shared_right_diff, recompute_impacted,
    recompute_keyed_impacted,
};
pub(super) use state::{RelationIndex, RelationIndexStorage};

// ── The public entry point ──────────────────────────────────────────────

/// Emit a batch of output diffs through `sink`.
///
/// Preserves the original `apply_batch` semantics observed by downstream
/// subscribers: every non-empty group of output diffs produced from a single
/// upstream diff is delivered as one `MapDiff::Batch`, even when the group
/// contains a single change. Empty batches are dropped.
fn emit_changes<OK, OV>(
    sink: &crate::map_query::BoxedMapDiffSink<OK, OV>,
    changes: Vec<MapDiff<OK, OV>>,
) where
    OK: Hash + Eq + CellValue,
    OV: CellValue,
{
    if changes.is_empty() {
        return;
    }
    sink(&MapDiff::Batch { changes });
}

/// Install join machinery that drives `sink` instead of allocating an output map.
///
/// Compiles both source maps into direct entry points, maintains the
/// join state, and pushes resulting `MapDiff`s into the sink. Returns the
/// subscription guards (caller owns them — typically attaches them to the
/// materialized output). Chains of plans compose without intermediate
/// [`CellMap`](crate::CellMap) allocations.
///
/// Used by `MapQuery` join plan nodes whose materialization owns a single
/// output cell map; multiple plan stages share that output rather than each
/// allocating their own.
pub fn install_join_runtime_via_query<LK, LV, RK, RV, JK, OK, OV, L, R, FL, FR, FO>(
    cx: &mut crate::map_query::compiler::CompileContext,
    left: L,
    right: R,
    left_join_key: FL,
    right_join_key: FR,
    compute_rows: FO,
    sink: crate::map_query::BoxedMapDiffSink<OK, OV>,
) -> Vec<SubscriptionGuard>
where
    LK: Hash + Eq + CellValue,
    LV: CellValue,
    RK: Hash + Eq + CellValue,
    RV: CellValue,
    JK: Hash + Eq + CellValue,
    OK: Hash + Eq + CellValue,
    OV: CellValue,
    L: crate::map_query::MapQuery<Key = LK, Value = LV>,
    R: crate::map_query::MapQuery<Key = RK, Value = RV>,
    FL: Fn(&LK, &LV) -> JK + Send + Sync + 'static,
    FR: RightJoinKey<RK, RV, JK> + Send + Sync + 'static,
    FO: Fn(&LK, &LV, &[(RK, RV)]) -> Vec<(OK, OV)> + Send + Sync + 'static,
{
    let state = Arc::new(Mutex::new(
        JoinState::<LK, LV, RK, RV, JK, OK, OV>::default(),
    ));
    let left_join_key = Arc::new(left_join_key);
    let right_join_key = Arc::new(right_join_key);
    let compute_rows = Arc::new(compute_rows);
    let sink = Arc::new(sink);

    let left_sink = {
        let state = state.clone();
        let left_join_key = left_join_key;
        let compute_rows = compute_rows.clone();
        let sink = sink.clone();
        move |diff: &MapDiff<LK, LV>| {
            let mut state = state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut scratch = std::mem::take(&mut state.scratch);
            apply_left_diff(
                &mut state,
                diff,
                left_join_key.as_ref(),
                &mut scratch.impacted,
            );
            let changes = recompute_impacted(&mut state, &mut scratch, compute_rows.as_ref());
            state.scratch = scratch;
            // Emit while STILL holding `state`, not after dropping it. Under the
            // scheduler's wave-parallel drain the left and right sinks can run on
            // two threads at once; each reads the sibling's rows under this lock
            // to build `changes`, so the emit must land under the same lock too.
            // Otherwise two concurrent sibling emits touching one output key can
            // reorder vs their lock order and last-write-wins a stale combined
            // row into the output map — the CellMap analogue of the join.rs
            // torn-value bug. Holding it across the emit makes emit order == lock
            // order, so whichever side observed the freshest sibling emits last.
            emit_changes(sink.as_ref(), changes);
            drop(state);
        }
    };

    let right_sink = {
        let state = state;
        let right_join_key = right_join_key;
        let compute_rows = compute_rows;
        let sink = sink;
        move |diff: &MapDiff<RK, RV>| {
            let mut state = state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut scratch = std::mem::take(&mut state.scratch);
            apply_right_diff(
                &mut state,
                diff,
                right_join_key.as_ref(),
                &mut scratch.impacted,
                &mut scratch.changed_join_keys,
            );
            let changes = recompute_impacted(&mut state, &mut scratch, compute_rows.as_ref());
            state.scratch = scratch;
            // Emit while STILL holding `state`, not after dropping it. Under the
            // scheduler's wave-parallel drain the left and right sinks can run on
            // two threads at once; each reads the sibling's rows under this lock
            // to build `changes`, so the emit must land under the same lock too.
            // Otherwise two concurrent sibling emits touching one output key can
            // reorder vs their lock order and last-write-wins a stale combined
            // row into the output map — the CellMap analogue of the join.rs
            // torn-value bug. Holding it across the emit makes emit order == lock
            // order, so whichever side observed the freshest sibling emits last.
            emit_changes(sink.as_ref(), changes);
            drop(state);
        }
    };

    let mut guards = crate::map_query::compile_runtime_into(left, cx, Arc::new(left_sink));
    guards.extend(crate::map_query::compile_runtime_into(
        right,
        cx,
        Arc::new(right_sink),
    ));
    guards
}

/// Install the zero-or-one-output, left-key-preserving join runtime.
pub fn install_keyed_join_runtime_via_query<LK, LV, RK, RV, JK, OV, L, R, FL, FR, FO>(
    cx: &mut crate::map_query::compiler::CompileContext,
    left: L,
    right: R,
    left_join_key: FL,
    right_join_key: FR,
    compute_value: FO,
    sink: crate::map_query::BoxedMapDiffSink<LK, OV>,
) -> Vec<SubscriptionGuard>
where
    LK: Hash + Eq + CellValue,
    LV: CellValue,
    RK: Hash + Eq + CellValue,
    RV: CellValue,
    JK: Hash + Eq + CellValue,
    OV: CellValue,
    L: crate::map_query::MapQuery<Key = LK, Value = LV>,
    R: crate::map_query::MapQuery<Key = RK, Value = RV>,
    FL: Fn(&LK, &LV) -> JK + Send + Sync + 'static,
    FR: RightJoinKey<RK, RV, JK> + Send + Sync + 'static,
    FO: Fn(&LK, &LV, &[(RK, RV)]) -> Option<OV> + Send + Sync + 'static,
{
    let relation = cx.take_relation_hint();
    // A relation marker alone is not enough to prove that two right inputs
    // have the same semantics. Only raw source boundaries have a stable
    // identity suitable for index reuse; projections, filters, and all other
    // operator nodes keep an independent index even when they share a root.
    if let Some(relation) = relation.filter(|_| right.raw_source_identity().is_some()) {
        let index = cx.prepare_relationship_index::<RelationIndex<RK, RV, JK>>();
        return install_keyed_join_runtime_with_index(
            cx,
            index.clone(),
            Some((relation, index)),
            left,
            right,
            left_join_key,
            right_join_key,
            compute_value,
            sink,
        );
    }

    install_keyed_join_runtime_with_index(
        cx,
        RelationIndex::default(),
        None,
        left,
        right,
        left_join_key,
        right_join_key,
        compute_value,
        sink,
    )
}

#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn install_keyed_join_runtime_with_index<LK, LV, RK, RV, JK, OV, L, R, FL, FR, FO, RI>(
    cx: &mut crate::map_query::compiler::CompileContext,
    right_index: RI,
    relationship_binding: Option<(
        TypeId,
        crate::map_query::compiler::DeferredPhysical<RelationIndex<RK, RV, JK>>,
    )>,
    left: L,
    right: R,
    left_join_key: FL,
    right_join_key: FR,
    compute_value: FO,
    sink: crate::map_query::BoxedMapDiffSink<LK, OV>,
) -> Vec<SubscriptionGuard>
where
    LK: Hash + Eq + CellValue,
    LV: CellValue,
    RK: Hash + Eq + CellValue,
    RV: CellValue,
    JK: Hash + Eq + CellValue,
    OV: CellValue,
    L: crate::map_query::MapQuery<Key = LK, Value = LV>,
    R: crate::map_query::MapQuery<Key = RK, Value = RV>,
    FL: Fn(&LK, &LV) -> JK + Send + Sync + 'static,
    FR: RightJoinKey<RK, RV, JK> + Send + Sync + 'static,
    FO: Fn(&LK, &LV, &[(RK, RV)]) -> Option<OV> + Send + Sync + 'static,
    RI: RelationIndexStorage<RK, RV, JK>,
{
    let shared_index = relationship_binding
        .as_ref()
        .map(|(_, index)| index.clone());
    let state = Arc::new(Mutex::new(
        JoinState::<LK, LV, RK, RV, JK, LK, OV, RI>::with_right(right_index),
    ));
    let left_join_key = Arc::new(left_join_key);
    let right_join_key = Arc::new(right_join_key);
    let compute_value = Arc::new(compute_value);
    let sink = Arc::new(sink);

    let left_sink = {
        let state = state.clone();
        let left_join_key = left_join_key;
        let compute_value = compute_value.clone();
        let sink = sink.clone();
        move |diff: &MapDiff<LK, LV>| {
            let mut state = state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut scratch = std::mem::take(&mut state.scratch);
            apply_left_diff(
                &mut state,
                diff,
                left_join_key.as_ref(),
                &mut scratch.impacted,
            );
            let changes =
                recompute_keyed_impacted(&mut state, &mut scratch, compute_value.as_ref());
            state.scratch = scratch;
            emit_changes(sink.as_ref(), changes);
            drop(state);
        }
    };

    let right_sink = {
        let state = state;
        let right_join_key = right_join_key;
        let compute_value = compute_value;
        let sink = sink;
        let shared_index = shared_index;
        move |diff: &MapDiff<RK, RV>| {
            let mut state = state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut scratch = std::mem::take(&mut state.scratch);
            if shared_index
                .as_ref()
                .is_none_or(crate::map_query::compiler::DeferredPhysical::maintains_index)
            {
                apply_right_diff(
                    &mut state,
                    diff,
                    right_join_key.as_ref(),
                    &mut scratch.impacted,
                    &mut scratch.changed_join_keys,
                );
            } else {
                observe_shared_right_diff(
                    &state,
                    diff,
                    right_join_key.as_ref(),
                    &mut scratch.impacted,
                    &mut scratch.changed_join_keys,
                );
            }
            let changes =
                recompute_keyed_impacted(&mut state, &mut scratch, compute_value.as_ref());
            state.scratch = scratch;
            emit_changes(sink.as_ref(), changes);
            drop(state);
        }
    };

    let mut guards = crate::map_query::compile_runtime_into(left, cx, Arc::new(left_sink));
    if let Some((relation, index)) = relationship_binding {
        guards.extend(cx.with_root_relation_index(relation, index, |cx| {
            crate::map_query::compile_runtime_into(right, cx, Arc::new(right_sink))
        }));
    } else {
        guards.extend(crate::map_query::compile_runtime_into(
            right,
            cx,
            Arc::new(right_sink),
        ));
    }
    guards
}

#[cfg(test)]
mod read_acquisition_tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use super::state::JoinScratch;
    use super::*;

    struct CountingIndex {
        index: RelationIndex<usize, usize, usize>,
        reads: Arc<AtomicUsize>,
    }

    impl RelationIndexStorage<usize, usize, usize> for CountingIndex {
        type Read<'a> = &'a RelationIndex<usize, usize, usize>;

        fn acquire_read(&self) -> Self::Read<'_> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            &self.index
        }

        fn write<T>(
            &mut self,
            write: impl FnOnce(&mut RelationIndex<usize, usize, usize>) -> T,
        ) -> T {
            write(&mut self.index)
        }
    }

    #[test]
    fn keyed_callback_acquires_shared_index_once_for_all_impacted_keys() {
        let reads = Arc::new(AtomicUsize::new(0));
        let mut index = RelationIndex::default();
        index.join_to_rows.insert(0, (0..32).collect());
        for key in 0..32 {
            index.rows.insert(key, key);
            index.row_join_keys.insert(key, 0);
        }
        let mut state = JoinState::with_right(CountingIndex {
            index,
            reads: Arc::clone(&reads),
        });
        let mut scratch = JoinScratch::default();
        for key in 0..128 {
            state.left_rows.insert(key, key);
            state.left_join_keys.insert(key, 0);
            scratch.impacted.insert(key);
        }

        let changes = recompute_keyed_impacted(&mut state, &mut scratch, &|_, _, right_rows| {
            Some(right_rows.len())
        });

        assert_eq!(changes.len(), 128);
        assert_eq!(reads.load(Ordering::Relaxed), 1);
        assert!(scratch.impacted_keys.is_empty());
        assert!(scratch.impacted_keys.capacity() >= 128);
    }
}
