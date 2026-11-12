use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use hyphae::{CellMap, LeftSemiJoinExt, MapDiff, MapQuery};

#[derive(Debug)]
struct CountedRow {
    group: usize,
    value: usize,
    clones: Arc<AtomicUsize>,
}
impl Clone for CountedRow {
    fn clone(&self) -> Self {
        self.clones.fetch_add(1, Ordering::Relaxed);
        Self {
            group: self.group,
            value: self.value,
            clones: self.clones.clone(),
        }
    }
}
impl PartialEq for CountedRow {
    fn eq(&self, other: &Self) -> bool {
        self.group == other.group && self.value == other.value
    }
}
impl Eq for CountedRow {}

#[test]
fn updating_one_left_row_does_not_clone_its_unchanged_bucket_siblings() {
    let clones = Arc::new(AtomicUsize::new(0));
    let right = CellMap::<usize, usize>::new();
    let left = CellMap::<usize, CountedRow>::new();
    left.insert_many(
        (0..4096)
            .map(|key| {
                (
                    key,
                    CountedRow {
                        group: 0,
                        value: key,
                        clones: clones.clone(),
                    },
                )
            })
            .collect(),
    );
    right.insert(0, 0);
    let joined = left
        .clone()
        .left_semi_join_by(right, |_, row| row.group, |_, group| *group)
        .materialize();
    assert_eq!(joined.snapshot().len(), 4096);
    clones.store(0, Ordering::Relaxed);
    left.insert(
        0,
        CountedRow {
            group: 0,
            value: 9000,
            clones: clones.clone(),
        },
    );
    assert_eq!(joined.get_value(&0).map(|row| row.value), Some(9000));
    let work = clones.load(Ordering::Relaxed);
    eprintln!("one changed row cloned {work} values in a 4096-row bucket");
    if cfg!(feature = "profiling")
        && std::env::var("HYPHAE_SEMI_JOIN_DELTA").is_ok_and(|v| v == "1")
    {
        assert!(
            work < 64,
            "one changed left row cloned {work} values from a 4096-row bucket"
        );
    }
}

#[test]
fn semi_deltas_match_a_snapshot_join_after_updates_moves_batches_and_resets() {
    use std::collections::BTreeMap;

    let left = CellMap::<usize, (usize, usize)>::new();
    let right = CellMap::<usize, (usize, usize)>::new();
    let joined = left
        .clone()
        .left_semi_join_by(right.clone(), |_, v| v.0, |_, v| v.0)
        .materialize();
    let mut random = 17_usize;
    for step in 0..200_usize {
        random = random.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let key = (random >> 4).checked_rem(7).unwrap_or(0);
        let value = ((random >> 8).checked_rem(4).unwrap_or(0), step);
        let source = if random & 1 == 0 { &left } else { &right };
        match step.checked_rem(5).unwrap_or(0) {
            0 => {
                source.insert(key, value);
            }
            1 => source.apply_batch(vec![
                MapDiff::Insert { key, value },
                MapDiff::Update {
                    key,
                    old_value: value,
                    new_value: (value.0, step.saturating_add(1)),
                },
            ]),
            2 => {
                source.remove(&key);
            }
            3 => source.apply_diff_owned(MapDiff::Initial {
                entries: vec![
                    (key, value),
                    (key.saturating_add(1), (value.0, step.saturating_add(2))),
                ],
            }),
            _ => source.apply_batch(vec![
                MapDiff::Remove {
                    key,
                    old_value: source.get_value(&key).unwrap_or(value),
                },
                MapDiff::Insert { key, value },
            ]),
        }
        let mut expected = BTreeMap::new();
        for (lk, lv) in left.snapshot() {
            for (_, rv) in right.snapshot() {
                if lv.0 == rv.0 {
                    expected.insert(lk, lv);
                }
            }
        }
        assert_eq!(
            joined.snapshot().into_iter().collect::<BTreeMap<_, _>>(),
            expected,
            "step {step}"
        );
    }
}

#[test]
fn duplicate_membership_and_value_only_right_updates_preserve_left_output() {
    let left = CellMap::<u32, u32>::new();
    let right = CellMap::<u32, u32>::new();
    left.insert(1, 10);
    right.insert(1, 10);
    right.insert(2, 10);
    let joined = left
        .left_semi_join_by(right.clone(), |_, v| *v, |_, v| *v)
        .materialize();
    right.remove(&1);
    assert_eq!(joined.get_value(&1), Some(10));
    right.apply_batch(vec![
        MapDiff::Remove {
            key: 2,
            old_value: 10,
        },
        MapDiff::Insert { key: 3, value: 10 },
    ]);
    assert_eq!(joined.get_value(&1), Some(10));
    right.insert(3, 20);
    assert_eq!(joined.get_value(&1), None);
    right.apply_diff_owned(MapDiff::Initial {
        entries: vec![(4, 10)],
    });
    assert_eq!(joined.get_value(&1), Some(10));
}

#[derive(Clone, Debug)]
struct Revision {
    number: u32,
    payload: &'static str,
}
impl PartialEq for Revision {
    fn eq(&self, other: &Self) -> bool {
        self.number == other.number
    }
}
#[test]
fn equal_source_reset_keeps_the_exact_published_old_value() {
    use std::sync::Mutex;
    let left = CellMap::<u32, Revision>::new();
    let right = CellMap::<u32, u32>::new();
    left.insert(
        1,
        Revision {
            number: 1,
            payload: "published",
        },
    );
    right.insert(1, 1);
    let joined = left.clone().left_semi_join(right).materialize();
    left.apply_diff_owned(MapDiff::Initial {
        entries: vec![(
            1,
            Revision {
                number: 1,
                payload: "equal replacement",
            },
        )],
    });
    assert_eq!(joined.get_value(&1).map(|v| v.payload), Some("published"));
    let old = Arc::new(Mutex::new(Vec::new()));
    let seen = old.clone();
    let _guard = joined.subscribe_diffs(move |diff| {
        capture(
            diff,
            &mut seen
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
    });
    left.insert(
        1,
        Revision {
            number: 2,
            payload: "new",
        },
    );
    assert_eq!(
        *old.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        vec!["published"]
    );
}

fn capture(diff: &MapDiff<u32, Revision>, old: &mut Vec<&'static str>) {
    match diff {
        MapDiff::Update { old_value, .. } => old.push(old_value.payload),
        MapDiff::Batch { changes } => {
            for change in changes {
                capture(change, old);
            }
        }
        _ => {}
    }
}

#[test]
fn changing_a_right_payload_without_membership_change_does_not_clone_left_rows() {
    let clones = Arc::new(AtomicUsize::new(0));
    let left = CellMap::<usize, CountedRow>::new();
    let right = CellMap::<usize, (usize, usize)>::new();
    left.insert_many(
        (0..4096)
            .map(|key| {
                (
                    key,
                    CountedRow {
                        group: 0,
                        value: key,
                        clones: clones.clone(),
                    },
                )
            })
            .collect(),
    );
    right.insert(0, (0, 1));
    let joined = left
        .left_semi_join_by(right.clone(), |_, row| row.group, |_, row| row.0)
        .materialize();
    assert_eq!(joined.snapshot().len(), 4096);
    clones.store(0, Ordering::Relaxed);
    right.insert(0, (0, 2));
    let work = clones.load(Ordering::Relaxed);
    eprintln!("right payload-only update cloned {work} left values");
    if cfg!(feature = "profiling")
        && std::env::var("HYPHAE_SEMI_JOIN_DELTA").is_ok_and(|v| v == "1")
    {
        assert_eq!(work, 0);
    }
    assert_eq!(joined.snapshot().len(), 4096);
}

#[test]
fn racing_roots_settle_to_their_final_snapshot() {
    let left = CellMap::<usize, (usize, usize)>::new();
    let right = CellMap::<usize, usize>::new();
    for key in 0..16 {
        left.insert(key, (key & 3, 0));
    }
    for key in 0..4 {
        right.insert(key, key);
    }
    let joined = left
        .clone()
        .left_semi_join_by(right.clone(), |_, v| v.0, |_, v| *v)
        .materialize();
    let barrier = Arc::new(std::sync::Barrier::new(2));
    std::thread::scope(|scope| {
        let left_barrier = barrier.clone();
        let left_source = left.clone();
        scope.spawn(move || {
            left_barrier.wait();
            for revision in 1..128_usize {
                let key = revision & 15;
                left_source.insert(key, (key & 3, revision));
                std::thread::yield_now();
            }
        });
        scope.spawn(|| {
            barrier.wait();
            for revision in 1..128_usize {
                let key = revision & 3;
                if revision & 1 == 0 {
                    right.insert(key, key);
                } else {
                    right.remove(&key);
                }
                std::thread::yield_now();
            }
            right.insert_many((0..4).map(|key| (key, key)).collect());
        });
    });
    hyphae::batch(|| {});
    assert_eq!(
        joined
            .snapshot()
            .into_iter()
            .collect::<std::collections::BTreeMap<_, _>>(),
        left.snapshot().into_iter().collect()
    );
}
