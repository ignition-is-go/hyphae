use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use hyphae::{CellMap, InnerJoinExt, MapDiff, MapQuery};

// An open batch defers propagation process-wide, including other tests' graphs.
static SCHEDULER_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn scheduler_test_serial() -> std::sync::MutexGuard<'static, ()> {
    SCHEDULER_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

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
fn updating_one_right_row_does_not_clone_its_unchanged_bucket_siblings() {
    let _serial = scheduler_test_serial();
    let clones = Arc::new(AtomicUsize::new(0));
    let left = CellMap::<usize, usize>::new();
    let right = CellMap::<usize, CountedRow>::new();
    right.insert_many(
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
    left.insert(0, 0);
    let joined = left
        .inner_join_by(right.clone(), |_, group| *group, |_, row| row.group)
        .materialize();
    assert_eq!(joined.snapshot().len(), 4096);
    clones.store(0, Ordering::Relaxed);
    right.insert(
        0,
        CountedRow {
            group: 0,
            value: 9000,
            clones: clones.clone(),
        },
    );
    assert_eq!(
        joined.get_value(&(0, 0)).map(|(_, row)| row.value),
        Some(9000)
    );
    let work = clones.load(Ordering::Relaxed);
    eprintln!("one changed row cloned {work} values in a 4096-row bucket");
    assert!(
        work < 64,
        "one changed right row cloned {work} values from a 4096-row bucket"
    );
}

#[test]
fn pair_deltas_match_a_snapshot_join_after_updates_moves_batches_and_resets() {
    use std::collections::BTreeMap;

    let _serial = scheduler_test_serial();
    let left = CellMap::<usize, (usize, usize)>::new();
    let right = CellMap::<usize, (usize, usize)>::new();
    let joined = left
        .clone()
        .inner_join_by(right.clone(), |_, v| v.0, |_, v| v.0)
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
            for (rk, rv) in right.snapshot() {
                if lv.0 == rv.0 {
                    expected.insert((lk, rk), (lv, rv));
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

#[derive(Clone, Debug)]
struct Revision {
    number: usize,
    payload: &'static str,
}
impl PartialEq for Revision {
    fn eq(&self, other: &Self) -> bool {
        self.number == other.number
    }
}

fn capture(diff: &MapDiff<(usize, usize), (Revision, usize)>, old: &mut Vec<&'static str>) {
    match diff {
        MapDiff::Update { old_value, .. } => old.push(old_value.0.payload),
        MapDiff::Batch { changes } => {
            for change in changes {
                capture(change, old);
            }
        }
        _ => {}
    }
}

#[test]
fn old_values_are_the_published_payload_after_an_equal_source_reset() {
    use std::sync::Mutex;

    let _serial = scheduler_test_serial();
    let left = CellMap::<usize, Revision>::new();
    let right = CellMap::<usize, usize>::new();
    left.insert(
        0,
        Revision {
            number: 1,
            payload: "published",
        },
    );
    right.insert(0, 0);
    let joined = left
        .clone()
        .inner_join_by(right, |_, _| 0, |_, _| 0)
        .materialize();
    left.apply_diff_owned(MapDiff::Initial {
        entries: vec![(
            0,
            Revision {
                number: 1,
                payload: "equal replacement",
            },
        )],
    });
    assert_eq!(
        joined.get_value(&(0, 0)).map(|v| v.0.payload),
        Some("published")
    );
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
        0,
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
    assert_eq!(joined.get_value(&(0, 0)).map(|v| v.0.payload), Some("new"));
}

#[cfg(feature = "scheduler")]
#[test]
fn concurrent_pair_roots_settle_on_the_latest_values() {
    let _serial = scheduler_test_serial();
    hyphae::scheduler::set_wave_threshold_for_test(4);
    let graphs: Vec<_> = (0..8)
        .map(|_| {
            let left = CellMap::<usize, usize>::new();
            let right = CellMap::<usize, usize>::new();
            left.insert(0, 0);
            right.insert(0, 0);
            let joined = left
                .clone()
                .inner_join_by(right.clone(), |_, _| 0, |_, _| 0)
                .materialize();
            (left, right, joined)
        })
        .collect();
    for iteration in 1..=400_usize {
        hyphae::batch(|| {
            for (index, (left, right, _)) in graphs.iter().enumerate() {
                let value = iteration.saturating_mul(16).saturating_add(index);
                left.insert(0, value);
                right.insert(0, value);
            }
        });
        for (index, (_, _, joined)) in graphs.iter().enumerate() {
            let value = iteration.saturating_mul(16).saturating_add(index);
            assert_eq!(
                joined.get_value(&(0, 0)),
                Some((value, value)),
                "iteration {iteration}, graph {index}"
            );
        }
    }
}
