//! Zero-or-one rekeys need ownership and published values, not a second copy
//! of every source value or a set of outputs per source row.
use std::{
    hash::Hash,
    sync::{Arc, Mutex},
};

use rustc_hash::FxHashMap;

use super::ordered_set::OrderedSet;
use crate::{
    cell_map::MapDiff,
    map_query::{BoxedMapDiffSink, MapQuery, compiler::CompileContext},
    subscription::SubscriptionGuard,
    traits::CellValue,
};

pub(super) struct State<SK, OK, OV> {
    source_order: OrderedSet<SK>,
    rows: FxHashMap<SK, (OK, OV)>,
    owners: FxHashMap<OK, SK>,
}
impl<SK, OK, OV> Default for State<SK, OK, OV> {
    fn default() -> Self {
        Self {
            source_order: OrderedSet::default(),
            rows: FxHashMap::default(),
            owners: FxHashMap::default(),
        }
    }
}

fn collect<SK, SV>(
    diff: &MapDiff<SK, SV>,
    source_order: &mut OrderedSet<SK>,
    order: &mut OrderedSet<SK>,
    pending: &mut FxHashMap<SK, Option<SV>>,
) where
    SK: Hash + Eq + CellValue,
    SV: CellValue,
{
    match diff {
        MapDiff::Batch { changes } => {
            for change in changes {
                collect(change, source_order, order, pending);
            }
        }
        MapDiff::Initial { entries } => {
            for value in pending.values_mut() {
                *value = None;
            }
            for key in source_order.drain_vec() {
                order.insert(key.clone());
                pending.insert(key, None);
            }
            for (key, value) in entries {
                source_order.insert(key.clone());
                order.insert(key.clone());
                pending.insert(key.clone(), Some(value.clone()));
            }
        }
        MapDiff::Insert { key, value }
        | MapDiff::Update {
            key,
            new_value: value,
            ..
        } => {
            source_order.insert(key.clone());
            order.insert(key.clone());
            pending.insert(key.clone(), Some(value.clone()));
        }
        MapDiff::Remove { key, .. } => {
            source_order.remove(key);
            order.insert(key.clone());
            pending.insert(key.clone(), None);
        }
    }
}

impl<SK, OK, OV> State<SK, OK, OV>
where
    SK: Hash + Eq + CellValue,
    OK: Hash + Eq + CellValue,
    OV: CellValue,
{
    fn point(&mut self, key: &SK, desired: Option<(OK, OV)>) -> Vec<MapDiff<OK, OV>> {
        if let Some((output, _)) = &desired {
            assert!(
                self.owners.get(output).is_none_or(|owner| owner == key),
                "map query unique-output-key contract violated by an existing source row"
            );
        }
        let mut changes = Vec::new();
        if let Some((old_key, _)) = self.rows.get(key)
            && desired.as_ref().is_none_or(|(next, _)| next != old_key)
            && let Some((old_key, old_value)) = self.rows.remove(key)
        {
            self.owners.remove(&old_key);
            changes.push(MapDiff::Remove {
                key: old_key,
                old_value,
            });
        }
        if let Some((output, value)) = desired {
            if let Some((_, old)) = self.rows.get_mut(key) {
                if old != &value {
                    let old_value = std::mem::replace(old, value.clone());
                    changes.push(MapDiff::Update {
                        key: output,
                        old_value,
                        new_value: value,
                    });
                }
            } else {
                self.owners.insert(output.clone(), key.clone());
                self.rows
                    .insert(key.clone(), (output.clone(), value.clone()));
                changes.push(MapDiff::Insert { key: output, value });
            }
        }
        changes
    }

    pub(super) fn apply<SV: CellValue>(
        &mut self,
        diff: &MapDiff<SK, SV>,
        compute: &impl Fn(&SK, &SV) -> Option<(OK, OV)>,
    ) -> Vec<MapDiff<OK, OV>> {
        match diff {
            MapDiff::Batch { changes } if changes.len() == 1 => {
                if let Some(change) = changes.first() {
                    return self.apply(change, compute);
                }
            }
            MapDiff::Insert { key, value }
            | MapDiff::Update {
                key,
                new_value: value,
                ..
            } => {
                let desired = compute(key, value);
                let changes = self.point(key, desired);
                self.source_order.insert(key.clone());
                return changes;
            }
            MapDiff::Remove { key, .. } => {
                let changes = self.point(key, None);
                self.source_order.remove(key);
                return changes;
            }
            _ => {}
        }
        let mut order = OrderedSet::default();
        let mut pending = FxHashMap::default();
        collect(diff, &mut self.source_order, &mut order, &mut pending);
        let mut claims = FxHashMap::default();
        let mut computed = Vec::with_capacity(pending.len());
        for key in order.drain_vec() {
            let desired = pending
                .get(&key)
                .and_then(Option::as_ref)
                .and_then(|v| compute(&key, v));
            if let Some((output, _)) = &desired {
                assert!(
                    claims.insert(output.clone(), key.clone()).is_none(),
                    "map query unique-output-key contract violated across source rows"
                );
                if let Some(owner) = self.owners.get(output) {
                    assert!(
                        owner == &key || pending.contains_key(owner),
                        "map query unique-output-key contract violated by an existing source row"
                    );
                }
            }
            computed.push((key, desired));
        }
        // Validate every claim before changing ownership; swaps may release a
        // key claimed by another row in the same upstream transaction.
        let mut changes = Vec::new();
        for (key, desired) in &computed {
            if let Some((old_key, _)) = self.rows.get(key)
                && desired
                    .as_ref()
                    .is_none_or(|(next_key, _)| next_key != old_key)
                && let Some((old_key, old_value)) = self.rows.remove(key)
            {
                self.owners.remove(&old_key);
                changes.push(MapDiff::Remove {
                    key: old_key,
                    old_value,
                });
            }
        }
        for (key, desired) in computed {
            let Some((output, value)) = desired else {
                continue;
            };
            if let Some((_, old)) = self.rows.get_mut(&key) {
                if old != &value {
                    let old_value = std::mem::replace(old, value.clone());
                    changes.push(MapDiff::Update {
                        key: output,
                        old_value,
                        new_value: value,
                    });
                }
            } else {
                self.owners.insert(output.clone(), key.clone());
                self.rows.insert(key, (output.clone(), value.clone()));
                changes.push(MapDiff::Insert { key: output, value });
            }
        }
        changes
    }
}

pub fn install_single_runtime<SK, SV, OK, OV, S, F>(
    cx: &mut CompileContext,
    source: S,
    compute: F,
    sink: BoxedMapDiffSink<OK, OV>,
) -> Vec<SubscriptionGuard>
where
    SK: Hash + Eq + CellValue,
    SV: CellValue,
    OK: Hash + Eq + CellValue,
    OV: CellValue,
    S: MapQuery<Key = SK, Value = SV>,
    F: Fn(&SK, &SV) -> Option<(OK, OV)> + Send + Sync + 'static,
{
    #[cfg(feature = "profiling")]
    let compute = move |key: &SK, value: &SV| {
        #[cfg(feature = "profiling")]
        tracing::trace!(target: "hyphae::work", work = "map_single", label = std::any::type_name::<SV>());
        compute(key, value)
    };
    let state = Arc::new(Mutex::new(State::<SK, OK, OV>::default()));
    crate::map_query::compile_runtime_into(
        source,
        cx,
        Arc::new(move |diff| {
            let changes = state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .apply(diff, &compute);
            if !changes.is_empty() {
                sink(&MapDiff::Batch { changes });
            }
        }),
    )
}

#[cfg(test)]
mod tests {
    use std::panic::{AssertUnwindSafe, catch_unwind};

    use super::*;

    #[test]
    fn collision_leaves_published_state_unchanged_and_can_recover() {
        let mut state = State::<u32, u32, i32>::default();
        let compute = |_: &u32, v: &i32| Some((v.unsigned_abs(), *v));
        state.apply(
            &MapDiff::Initial {
                entries: vec![(1, 10), (2, 20)],
            },
            &compute,
        );
        let result = catch_unwind(AssertUnwindSafe(|| {
            state.apply(
                &MapDiff::Update {
                    key: 1,
                    old_value: 10,
                    new_value: 20,
                },
                &compute,
            )
        }));
        assert!(result.is_err());
        assert_eq!(state.rows.get(&1), Some(&(10, 10)));
        assert_eq!(state.rows.get(&2), Some(&(20, 20)));
        state.apply(
            &MapDiff::Update {
                key: 1,
                old_value: 10,
                new_value: 30,
            },
            &compute,
        );
        assert_eq!(state.rows.get(&1), Some(&(30, 30)));
    }

    #[test]
    fn nested_batch_initial_resets_pending_rows_and_swaps_atomically() {
        let mut state = State::<u32, u32, i32>::default();
        let compute = |_: &u32, v: &i32| (*v > 0).then_some((v.unsigned_abs(), *v));
        state.apply(
            &MapDiff::Initial {
                entries: vec![(1, 10), (2, 20), (3, -1)],
            },
            &compute,
        );
        state.apply(
            &MapDiff::Batch {
                changes: vec![
                    MapDiff::Insert { key: 4, value: 40 },
                    MapDiff::Batch {
                        changes: vec![MapDiff::Initial {
                            entries: vec![(1, 20), (2, 10), (3, 30)],
                        }],
                    },
                    MapDiff::Remove {
                        key: 3,
                        old_value: 30,
                    },
                ],
            },
            &compute,
        );
        assert_eq!(state.rows.len(), 2);
        assert_eq!(state.rows.get(&1), Some(&(20, 20)));
        assert_eq!(state.rows.get(&2), Some(&(10, 10)));
        assert_eq!(state.owners.len(), 2);
    }

    #[derive(Clone, Debug)]
    struct Value {
        eq: u32,
        identity: u32,
    }
    impl PartialEq for Value {
        fn eq(&self, other: &Self) -> bool {
            self.eq == other.eq
        }
    }

    #[test]
    fn equal_projection_retains_identity_for_later_old_value() {
        let mut state = State::<u32, u32, Value>::default();
        let compute = |key: &u32, value: &Value| Some((*key, value.clone()));
        state.apply(
            &MapDiff::Insert {
                key: 1,
                value: Value {
                    eq: 1,
                    identity: 10,
                },
            },
            &compute,
        );
        assert!(
            state
                .apply(
                    &MapDiff::Update {
                        key: 1,
                        old_value: Value {
                            eq: 1,
                            identity: 10
                        },
                        new_value: Value {
                            eq: 1,
                            identity: 20
                        }
                    },
                    &compute
                )
                .is_empty()
        );
        let changes = state.apply(
            &MapDiff::Remove {
                key: 1,
                old_value: Value {
                    eq: 1,
                    identity: 20,
                },
            },
            &compute,
        );
        assert!(
            matches!(changes.first(), Some(MapDiff::Remove { old_value, .. }) if old_value.identity == 10)
        );
    }
}
