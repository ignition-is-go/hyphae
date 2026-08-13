//! Allocation profile for statically compiled `MapQuery` work.
//!
//! This file is copied into a selected revision by
//! `bench-map-query-allocations.sh` so the runtime commit and harness identity
//! are recorded independently.

use std::{
    alloc::{GlobalAlloc, Layout, System},
    hint::black_box,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Instant,
};

use hyphae::{
    CellMap, MapQuery,
    traits::{LeftJoinExt, ProjectMapExt, SelectExt},
};

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}
fn rows() -> u64 {
    env_u64("HYPHAE_EVIDENCE_ROWS", 1_000)
}
fn single_updates() -> u64 {
    env_u64("HYPHAE_EVIDENCE_SINGLE_UPDATES", 100)
}
fn batch_size() -> u64 {
    env_u64("HYPHAE_EVIDENCE_BATCH", 100)
}

struct CountingAllocator;

static ALLOC_CALLS: AtomicU64 = AtomicU64::new(0);
static ALLOC_BYTES: AtomicU64 = AtomicU64::new(0);
static DEALLOC_CALLS: AtomicU64 = AtomicU64::new(0);
static DEALLOC_BYTES: AtomicU64 = AtomicU64::new(0);

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
            ALLOC_BYTES.fetch_add(
                u64::try_from(layout.size()).unwrap_or(u64::MAX),
                Ordering::Relaxed,
            );
        }
        pointer
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc_zeroed(layout) };
        if !pointer.is_null() {
            ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
            ALLOC_BYTES.fetch_add(
                u64::try_from(layout.size()).unwrap_or(u64::MAX),
                Ordering::Relaxed,
            );
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        DEALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
        DEALLOC_BYTES.fetch_add(
            u64::try_from(layout.size()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
        unsafe { System.dealloc(pointer, layout) };
    }

    unsafe fn realloc(&self, pointer: *mut u8, old: Layout, new_size: usize) -> *mut u8 {
        let replacement = unsafe { System.realloc(pointer, old, new_size) };
        if !replacement.is_null() {
            DEALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
            DEALLOC_BYTES.fetch_add(
                u64::try_from(old.size()).unwrap_or(u64::MAX),
                Ordering::Relaxed,
            );
            ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
            ALLOC_BYTES.fetch_add(
                u64::try_from(new_size).unwrap_or(u64::MAX),
                Ordering::Relaxed,
            );
        }
        replacement
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

#[derive(Clone, Debug, PartialEq)]
struct Row {
    relation: u64,
    payload: u64,
    generation: u64,
}

#[derive(Clone, Debug, PartialEq)]
struct Dimension {
    relation: u64,
    payload: u64,
}

#[derive(Clone, Copy)]
struct Snapshot {
    alloc_calls: u64,
    alloc_bytes: u64,
    dealloc_calls: u64,
    dealloc_bytes: u64,
}

impl Snapshot {
    fn live_bytes(self) -> i128 {
        i128::from(self.alloc_bytes).saturating_sub(i128::from(self.dealloc_bytes))
    }

    fn now() -> Self {
        Self {
            alloc_calls: ALLOC_CALLS.load(Ordering::Relaxed),
            alloc_bytes: ALLOC_BYTES.load(Ordering::Relaxed),
            dealloc_calls: DEALLOC_CALLS.load(Ordering::Relaxed),
            dealloc_bytes: DEALLOC_BYTES.load(Ordering::Relaxed),
        }
    }
}

#[derive(Clone, Copy)]
struct Measurement {
    phase: &'static str,
    alloc_calls: u64,
    alloc_bytes: u64,
    dealloc_calls: u64,
    dealloc_bytes: u64,
    elapsed_ns: u128,
    operations: u64,
    live_bytes_before: i128,
    live_bytes_after: i128,
}

impl Measurement {
    fn between(
        phase: &'static str,
        before: Snapshot,
        after: Snapshot,
        elapsed_ns: u128,
        operations: u64,
    ) -> Self {
        Self {
            phase,
            alloc_calls: after.alloc_calls.saturating_sub(before.alloc_calls),
            alloc_bytes: after.alloc_bytes.saturating_sub(before.alloc_bytes),
            dealloc_calls: after.dealloc_calls.saturating_sub(before.dealloc_calls),
            dealloc_bytes: after.dealloc_bytes.saturating_sub(before.dealloc_bytes),
            elapsed_ns,
            operations,
            live_bytes_before: before.live_bytes(),
            live_bytes_after: after.live_bytes(),
        }
    }

    fn net_bytes(self) -> i128 {
        i128::from(self.alloc_bytes).saturating_sub(i128::from(self.dealloc_bytes))
    }
}

fn source_rows() -> CellMap<u64, Arc<Row>> {
    let source = CellMap::new();
    for key in 0..rows() {
        source.insert(
            key,
            Arc::new(Row {
                relation: key % 64,
                payload: key.saturating_mul(17),
                generation: 0,
            }),
        );
    }
    source
}

fn dimensions(salt: u64) -> CellMap<u64, Arc<Dimension>> {
    let source = CellMap::new();
    for key in 0..rows() {
        source.insert(
            key,
            Arc::new(Dimension {
                relation: key % 64,
                payload: key.saturating_mul(31).wrapping_add(salt),
            }),
        );
    }
    source
}

fn updated_row(key: u64, generation: u64) -> Arc<Row> {
    Arc::new(Row {
        relation: key % 64,
        payload: key
            .saturating_mul(17)
            .wrapping_add(generation.saturating_mul(2)),
        generation,
    })
}

fn fold_matches(row: &Row, matches: &[Arc<Dimension>], salt: u64) -> Arc<Row> {
    let (payload_sum, match_count) = matches
        .iter()
        .fold((0_u64, 0_u64), |(sum, count), dimension| {
            (sum.wrapping_add(dimension.payload), count.wrapping_add(1))
        });
    let payload = row.payload.rotate_left(3)
        ^ payload_sum.rotate_left(11)
        ^ salt.wrapping_mul(match_count).rotate_left(19)
        ^ match_count.wrapping_mul(0x9e37_79b9_7f4a_7c15);
    Arc::new(Row {
        relation: row.relation,
        payload,
        generation: row.generation,
    })
}

fn measure_phase(
    phase: &'static str,
    before: Snapshot,
    started: Instant,
    operations: u64,
) -> (Snapshot, Measurement) {
    let elapsed = started.elapsed().as_nanos();
    let after = Snapshot::now();
    (
        after,
        Measurement::between(phase, before, after, elapsed, operations),
    )
}

fn print_measurements(
    revision: &str,
    scenario: &str,
    measurements: &[Measurement],
    output_rows: usize,
    output_checksum: u64,
) {
    for measurement in measurements {
        println!(
            "MAP_QUERY_ALLOCATION_CSV {revision},{scenario},{},{},{},{},{},{},{},{},{},{},{},{},{},{}",
            rows(),
            batch_size(),
            measurement.phase,
            measurement.alloc_calls,
            measurement.alloc_bytes,
            measurement.dealloc_calls,
            measurement.dealloc_bytes,
            measurement.net_bytes(),
            measurement.live_bytes_before,
            measurement.live_bytes_after,
            measurement.elapsed_ns,
            measurement.operations,
            output_rows,
            output_checksum,
        );
    }
}

#[inline(never)]
fn map_query_codegen_probe<F: FnOnce()>(update: F) {
    update();
}

fn measure_projection(revision: &str) {
    let scenario_baseline = Snapshot::now();
    let started = Instant::now();
    let source = source_rows();
    let (after_setup, setup) = measure_phase("setup", scenario_baseline, started, 1);
    let started = Instant::now();
    let plan = source
        .clone()
        .select(|row| row.payload % 2 == 0)
        .project(|key, row| {
            Some((
                *key,
                Arc::new(Row {
                    relation: row.relation,
                    payload: row.payload.rotate_left(7),
                    generation: row.generation,
                }),
            ))
        })
        .select(|row| row.relation < 64)
        .project(|key, row| {
            Some((
                *key,
                Arc::new(Row {
                    relation: row.relation,
                    payload: row.payload.wrapping_mul(33),
                    generation: row.generation,
                }),
            ))
        });
    let (after_build, build) = measure_phase("build", after_setup, started, 1);

    let started = Instant::now();
    let output = plan.materialize();
    let (after_materialize, materialize) = measure_phase("materialize", after_build, started, 1);

    let started = Instant::now();
    for generation in 1..=single_updates() {
        map_query_codegen_probe(|| {
            source.insert(0, updated_row(0, generation));
        });
        black_box(output.get_value(&0));
    }
    let (after_updates, updates) = measure_phase(
        "single_updates",
        after_materialize,
        started,
        single_updates(),
    );

    let started = Instant::now();
    source.insert_many(
        (0..batch_size())
            .map(|key| (key, updated_row(key, single_updates().wrapping_add(1))))
            .collect(),
    );
    black_box(output.get_value(&0));
    let (after_batch, batch) = measure_phase("batch", after_updates, started, 1);

    let snapshot = output.snapshot();
    let output_rows = snapshot.len();
    let expected_rows =
        usize::try_from(rows().max(batch_size()).div_ceil(2)).expect("row count fits usize");
    assert_eq!(
        output_rows, expected_rows,
        "projection_region output cardinality"
    );
    let mut output_entries: Vec<_> = snapshot.iter().collect();
    output_entries.sort_by_key(|(key, _)| *key);
    let output_checksum =
        output_entries
            .into_iter()
            .fold(0xcbf29ce484222325_u64, |sum, (key, row)| {
                sum.rotate_left(9)
                    ^ key.rotate_left(7)
                    ^ row.payload.rotate_left(17)
                    ^ row.generation
            });
    drop(snapshot);
    let before_teardown = Snapshot::now();
    let started = Instant::now();
    drop(output);
    drop(source);
    let (after_teardown, teardown) = measure_phase("teardown", before_teardown, started, 1);
    assert_eq!(
        after_teardown.live_bytes(),
        scenario_baseline.live_bytes(),
        "scenario teardown must return to baseline"
    );

    print_measurements(
        revision,
        "projection_region",
        &[setup, build, materialize, updates, batch, teardown],
        output_rows,
        output_checksum,
    );
    black_box(after_batch);
}

fn measure_two_join(revision: &str) {
    let scenario_baseline = Snapshot::now();
    let started = Instant::now();
    let source = source_rows();
    let first = dimensions(17);
    let second = dimensions(19);
    let (after_setup, setup) = measure_phase("setup", scenario_baseline, started, 1);
    let started = Instant::now();
    let plan = source
        .clone()
        .left_join_by(
            first,
            |_key, row| row.relation,
            |_key, dimension| dimension.relation,
        )
        .project(|key, (row, matches)| Some((*key, fold_matches(row, matches, 17))))
        .left_join_by(
            second,
            |_key, row| row.relation,
            |_key, dimension| dimension.relation,
        )
        .project(|key, (row, matches)| Some((*key, fold_matches(row, matches, 19))));
    let (after_build, build) = measure_phase("build", after_setup, started, 1);

    let started = Instant::now();
    let output = plan.materialize();
    let (after_materialize, materialize) = measure_phase("materialize", after_build, started, 1);

    let started = Instant::now();
    for generation in 1..=single_updates() {
        source.insert(0, updated_row(0, generation));
        black_box(output.get_value(&0));
    }
    let (after_updates, updates) = measure_phase(
        "single_updates",
        after_materialize,
        started,
        single_updates(),
    );

    let started = Instant::now();
    source.insert_many(
        (0..batch_size())
            .map(|key| (key, updated_row(key, single_updates().wrapping_add(1))))
            .collect(),
    );
    black_box(output.get_value(&0));
    let (_, batch) = measure_phase("batch", after_updates, started, 1);

    let snapshot = output.snapshot();
    let output_rows = snapshot.len();
    let expected_rows = usize::try_from(rows().max(batch_size())).expect("row count fits usize");
    assert_eq!(
        output_rows, expected_rows,
        "two_join_region output cardinality"
    );
    let mut output_entries: Vec<_> = snapshot.iter().collect();
    output_entries.sort_by_key(|(key, _)| *key);
    let output_checksum =
        output_entries
            .into_iter()
            .fold(0xcbf29ce484222325_u64, |sum, (key, row)| {
                sum.rotate_left(9)
                    ^ key.rotate_left(7)
                    ^ row.payload.rotate_left(17)
                    ^ row.generation
            });
    drop(snapshot);
    let before_teardown = Snapshot::now();
    let started = Instant::now();
    drop(output);
    drop(source);
    let (after_teardown, teardown) = measure_phase("teardown", before_teardown, started, 1);
    assert_eq!(
        after_teardown.live_bytes(),
        scenario_baseline.live_bytes(),
        "scenario teardown must return to baseline"
    );

    print_measurements(
        revision,
        "two_join_region",
        &[setup, build, materialize, updates, batch, teardown],
        output_rows,
        output_checksum,
    );
}

fn measure_four_join(revision: &str) {
    let scenario_baseline = Snapshot::now();
    let started = Instant::now();
    let source = source_rows();
    let shared = dimensions(23);
    let shared_second = dimensions(29);
    let (after_setup, setup) = measure_phase("setup", scenario_baseline, started, 1);
    let started = Instant::now();
    let plan = source
        .clone()
        .left_join_by(
            shared.clone(),
            |key, _row| *key,
            |_key, dimension| dimension.relation,
        )
        .project(|key, (row, matches)| Some((*key, fold_matches(row, matches, 1))))
        .left_join_by(
            shared_second.clone(),
            |key, _row| *key,
            |_key, dimension| dimension.relation,
        )
        .project(|key, (row, matches)| Some((*key, fold_matches(row, matches, 2))))
        .left_join_by(
            shared,
            |key, _row| *key,
            |_key, dimension| dimension.relation,
        )
        .project(|key, (row, matches)| Some((*key, fold_matches(row, matches, 3))))
        .left_join_by(
            shared_second,
            |key, _row| *key,
            |_key, dimension| dimension.relation,
        )
        .project(|key, (row, matches)| Some((*key, fold_matches(row, matches, 4))));
    let (after_build, build) = measure_phase("build", after_setup, started, 1);

    let started = Instant::now();
    let output = plan.materialize();
    let (after_materialize, materialize) = measure_phase("materialize", after_build, started, 1);

    let started = Instant::now();
    for generation in 1..=single_updates() {
        source.insert(0, updated_row(0, generation));
        black_box(output.get_value(&0));
    }
    let (after_updates, updates) = measure_phase(
        "single_updates",
        after_materialize,
        started,
        single_updates(),
    );

    let started = Instant::now();
    source.insert_many(
        (0..batch_size())
            .map(|key| (key, updated_row(key, single_updates().wrapping_add(1))))
            .collect(),
    );
    black_box(output.get_value(&0));
    let (_, batch) = measure_phase("batch", after_updates, started, 1);

    let snapshot = output.snapshot();
    let output_rows = snapshot.len();
    let expected_rows = usize::try_from(rows().max(batch_size())).expect("row count fits usize");
    assert_eq!(
        output_rows, expected_rows,
        "repeated_relation_four_join output cardinality"
    );
    let mut output_entries: Vec<_> = snapshot.iter().collect();
    output_entries.sort_by_key(|(key, _)| *key);
    let output_checksum =
        output_entries
            .into_iter()
            .fold(0xcbf29ce484222325_u64, |sum, (key, row)| {
                sum.rotate_left(9)
                    ^ key.rotate_left(7)
                    ^ row.payload.rotate_left(17)
                    ^ row.generation
            });
    drop(snapshot);
    let before_teardown = Snapshot::now();
    let started = Instant::now();
    drop(output);
    drop(source);
    let (after_teardown, teardown) = measure_phase("teardown", before_teardown, started, 1);
    assert_eq!(
        after_teardown.live_bytes(),
        scenario_baseline.live_bytes(),
        "scenario teardown must return to baseline"
    );

    print_measurements(
        revision,
        "repeated_relation_four_join",
        &[setup, build, materialize, updates, batch, teardown],
        output_rows,
        output_checksum,
    );
}

fn measure_rekey(revision: &str) {
    let scenario_baseline = Snapshot::now();
    let started = Instant::now();
    let source = source_rows();
    let first = dimensions(11);
    let second = dimensions(13);
    let (after_setup, setup) = measure_phase("setup", scenario_baseline, started, 1);
    let started = Instant::now();
    let plan = source
        .clone()
        .left_join_by(first, |_key, row| row.relation, |_key, d| d.relation)
        .project(|key, (row, matches)| {
            Some((key.wrapping_add(rows()), fold_matches(row, matches, 11)))
        })
        .left_join_by(second, |_key, row| row.relation, |_key, d| d.relation)
        .project(|key, (row, matches)| Some((*key, fold_matches(row, matches, 13))));
    let (after_build, build) = measure_phase("build", after_setup, started, 1);
    let started = Instant::now();
    let output = plan.materialize();
    let (after_materialize, materialize) = measure_phase("materialize", after_build, started, 1);
    let started = Instant::now();
    for generation in 1..=single_updates() {
        source.insert(0, updated_row(0, generation));
        black_box(output.get_value(&rows()));
    }
    let (after_updates, updates) = measure_phase(
        "single_updates",
        after_materialize,
        started,
        single_updates(),
    );
    let started = Instant::now();
    source.insert_many(
        (0..batch_size())
            .map(|key| (key, updated_row(key, single_updates().wrapping_add(1))))
            .collect(),
    );
    black_box(output.get_value(&rows()));
    let (_, batch) = measure_phase("batch", after_updates, started, 1);
    let snapshot = output.snapshot();
    let output_rows = snapshot.len();
    let expected_rows = usize::try_from(rows().max(batch_size())).expect("row count fits usize");
    assert_eq!(
        output_rows, expected_rows,
        "rekey_between_joins output cardinality"
    );
    let mut output_entries: Vec<_> = snapshot.iter().collect();
    output_entries.sort_by_key(|(key, _)| *key);
    let output_checksum =
        output_entries
            .into_iter()
            .fold(0xcbf29ce484222325_u64, |sum, (key, row)| {
                sum.rotate_left(9)
                    ^ key.rotate_left(7)
                    ^ row.payload.rotate_left(17)
                    ^ row.generation
            });
    drop(snapshot);
    let before_teardown = Snapshot::now();
    let started = Instant::now();
    drop(output);
    drop(source);
    let (_, teardown) = measure_phase("teardown", before_teardown, started, 1);
    print_measurements(
        revision,
        "rekey_between_joins",
        &[setup, build, materialize, updates, batch, teardown],
        output_rows,
        output_checksum,
    );
}

fn main() {
    let revision = option_env!("HYPHAE_BENCH_REVISION").unwrap_or("unknown");
    println!(
        "MAP_QUERY_ALLOCATION_CSV revision,scenario,rows,batch_size,phase,alloc_calls,alloc_bytes,dealloc_calls,dealloc_bytes,net_bytes,live_bytes_before,live_bytes_after,elapsed_ns,operations,output_rows,output_checksum"
    );
    match std::env::var("HYPHAE_EVIDENCE_SCENARIO").as_deref() {
        Ok("projection_region") => measure_projection(revision),
        Ok("two_join_region") => measure_two_join(revision),
        Ok("repeated_relation_four_join") => measure_four_join(revision),
        Ok("rekey_between_joins") => measure_rekey(revision),
        Ok(other) => panic!("unknown HYPHAE_EVIDENCE_SCENARIO={other}"),
        Err(_) => {
            measure_projection(revision);
            measure_two_join(revision);
            measure_four_join(revision);
            measure_rekey(revision);
        }
    }
}
