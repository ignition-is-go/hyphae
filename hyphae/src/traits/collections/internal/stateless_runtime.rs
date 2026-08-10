//! Stateless, key-preserving collection kernels.
//!
//! Incoming [`MapDiff`] values already contain the old and new row values
//! required to transform a key-preserving projection. These runtimes therefore
//! need no source mirror, output-key index, cache, mutex, or per-row hash set.

use std::hash::Hash;

use crate::{
    cell_map::MapDiff,
    map_query::{MapDiffSink, MapQuery},
    subscription::SubscriptionGuard,
    traits::CellValue,
};

fn map_diff<K, V, U, F>(diff: &MapDiff<K, V>, f: &F) -> Option<MapDiff<K, U>>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
    U: CellValue,
    F: Fn(&K, &V) -> U,
{
    match diff {
        MapDiff::Initial { entries } => Some(MapDiff::Initial {
            entries: entries
                .iter()
                .map(|(key, value)| (key.clone(), f(key, value)))
                .collect(),
        }),
        MapDiff::Insert { key, value } => Some(MapDiff::Insert {
            key: key.clone(),
            value: f(key, value),
        }),
        MapDiff::Remove { key, old_value } => Some(MapDiff::Remove {
            key: key.clone(),
            old_value: f(key, old_value),
        }),
        MapDiff::Update {
            key,
            old_value,
            new_value,
        } => {
            let old_value = f(key, old_value);
            let new_value = f(key, new_value);
            (old_value != new_value).then(|| MapDiff::Update {
                key: key.clone(),
                old_value,
                new_value,
            })
        }
        MapDiff::Batch { changes } => {
            let changes: Vec<_> = changes
                .iter()
                .filter_map(|change| map_diff(change, f))
                .collect();
            (!changes.is_empty()).then_some(MapDiff::Batch { changes })
        }
    }
}

fn filter_map_diff<K, V, U, F>(diff: &MapDiff<K, V>, f: &F) -> Option<MapDiff<K, U>>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
    U: CellValue,
    F: Fn(&K, &V) -> Option<U>,
{
    match diff {
        MapDiff::Initial { entries } => Some(MapDiff::Initial {
            entries: entries
                .iter()
                .filter_map(|(key, value)| f(key, value).map(|value| (key.clone(), value)))
                .collect(),
        }),
        MapDiff::Insert { key, value } => f(key, value).map(|value| MapDiff::Insert {
            key: key.clone(),
            value,
        }),
        MapDiff::Remove { key, old_value } => f(key, old_value).map(|old_value| MapDiff::Remove {
            key: key.clone(),
            old_value,
        }),
        MapDiff::Update {
            key,
            old_value,
            new_value,
        } => match (f(key, old_value), f(key, new_value)) {
            (Some(old_value), Some(new_value)) if old_value != new_value => Some(MapDiff::Update {
                key: key.clone(),
                old_value,
                new_value,
            }),
            (Some(old_value), None) => Some(MapDiff::Remove {
                key: key.clone(),
                old_value,
            }),
            (None, Some(value)) => Some(MapDiff::Insert {
                key: key.clone(),
                value,
            }),
            (Some(_), Some(_)) | (None, None) => None,
        },
        MapDiff::Batch { changes } => {
            let changes: Vec<_> = changes
                .iter()
                .filter_map(|change| filter_map_diff(change, f))
                .collect();
            (!changes.is_empty()).then_some(MapDiff::Batch { changes })
        }
    }
}

pub fn install_map_values_runtime<K, V, U, S, F, Sink>(
    cx: &mut crate::map_query::compiler::CompileContext,
    source: S,
    f: F,
    sink: Sink,
) -> Vec<SubscriptionGuard>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
    U: CellValue,
    S: MapQuery<Key = K, Value = V>,
    F: Fn(&K, &V) -> U + Send + Sync + 'static,
    Sink: MapDiffSink<K, U>,
{
    let upstream_sink = move |diff: &MapDiff<K, V>| {
        if let Some(mapped) = map_diff(diff, &f) {
            sink(&mapped);
        }
    };
    source.compile_into(cx, upstream_sink)
}

pub fn install_filter_map_values_runtime<K, V, U, S, F, Sink>(
    cx: &mut crate::map_query::compiler::CompileContext,
    source: S,
    f: F,
    sink: Sink,
) -> Vec<SubscriptionGuard>
where
    K: Hash + Eq + CellValue,
    V: CellValue,
    U: CellValue,
    S: MapQuery<Key = K, Value = V>,
    F: Fn(&K, &V) -> Option<U> + Send + Sync + 'static,
    Sink: MapDiffSink<K, U>,
{
    let upstream_sink = move |diff: &MapDiff<K, V>| {
        if let Some(mapped) = filter_map_diff(diff, &f) {
            sink(&mapped);
        }
    };
    source.compile_into(cx, upstream_sink)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_map_update_crosses_membership_boundary() {
        let f = |_key: &u64, value: &u64| (*value >= 10).then_some(*value);
        let entering = filter_map_diff(
            &MapDiff::Update {
                key: 1,
                old_value: 5,
                new_value: 10,
            },
            &f,
        );
        assert_eq!(entering, Some(MapDiff::Insert { key: 1, value: 10 }));

        let leaving = filter_map_diff(
            &MapDiff::Update {
                key: 1,
                old_value: 10,
                new_value: 5,
            },
            &f,
        );
        assert_eq!(
            leaving,
            Some(MapDiff::Remove {
                key: 1,
                old_value: 10
            })
        );
    }
}
