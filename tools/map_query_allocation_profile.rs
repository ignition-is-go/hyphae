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
    traits::{LeftJoinExt, MapValuesExt, SelectExt},
};

const ROWS: u64 = 1_000;
const SINGLE_UPDATES: u64 = 100;
const BATCH_SIZE: u64 = 100;

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
        }
    }

    fn net_bytes(self) -> i128 {
        i128::from(self.alloc_bytes).saturating_sub(i128::from(self.dealloc_bytes))
    }
}

fn source_rows() -> CellMap<u64, Arc<Row>> {
    let source = CellMap::new();
    for key in 0..ROWS {
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
    for key in 0..ROWS {
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
    let payload = matches.iter().fold(row.payload, |acc, dimension| {
        acc.rotate_left(5) ^ dimension.payload.wrapping_add(salt)
    });
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

fn print_measurements(revision: &str, scenario: &str, measurements: &[Measurement], rows: usize) {
    for measurement in measurements {
        println!(
            "MAP_QUERY_ALLOCATION_CSV {revision},{scenario},{},{},{},{},{},{},{},{},{}",
            measurement.phase,
            measurement.alloc_calls,
            measurement.alloc_bytes,
            measurement.dealloc_calls,
            measurement.dealloc_bytes,
            measurement.net_bytes(),
            measurement.elapsed_ns,
            measurement.operations,
            rows,
        );
    }
}

fn measure_projection(revision: &str) {
    let source = source_rows();
    let before_build = Snapshot::now();
    let started = Instant::now();
    let plan = source
        .clone()
        .select(|row| row.payload % 2 == 0)
        .map_values(|_key, row| {
            Arc::new(Row {
                relation: row.relation,
                payload: row.payload.rotate_left(7),
                generation: row.generation,
            })
        })
        .select(|row| row.relation < 64)
        .map_values(|_key, row| {
            Arc::new(Row {
                relation: row.relation,
                payload: row.payload.wrapping_mul(33),
                generation: row.generation,
            })
        });
    let (after_build, build) = measure_phase("build", before_build, started, 1);

    let started = Instant::now();
    let output = plan.materialize();
    let (after_materialize, materialize) = measure_phase("materialize", after_build, started, 1);

    let started = Instant::now();
    for generation in 1..=SINGLE_UPDATES {
        source.insert(0, updated_row(0, generation));
        black_box(output.get_value(&0));
    }
    let (after_updates, updates) =
        measure_phase("single_updates", after_materialize, started, SINGLE_UPDATES);

    let started = Instant::now();
    source.insert_many(
        (0..BATCH_SIZE)
            .map(|key| (key, updated_row(key, SINGLE_UPDATES.wrapping_add(1))))
            .collect(),
    );
    black_box(output.get_value(&0));
    let (after_batch, batch) = measure_phase("batch_100", after_updates, started, 1);

    let output_rows = output.snapshot().len();
    let before_teardown = Snapshot::now();
    let started = Instant::now();
    drop(output);
    let (_, teardown) = measure_phase("teardown", before_teardown, started, 1);

    print_measurements(
        revision,
        "projection_region",
        &[build, materialize, updates, batch, teardown],
        output_rows,
    );
    black_box(after_batch);
}

fn measure_two_join(revision: &str) {
    let source = source_rows();
    let first = dimensions(17);
    let second = dimensions(19);
    let before_build = Snapshot::now();
    let started = Instant::now();
    let plan = source
        .clone()
        .left_join_by(
            first,
            |_key, row| row.relation,
            |_key, dimension| dimension.relation,
        )
        .map_values(|_key, (row, matches)| fold_matches(row, matches, 17))
        .left_join_by(
            second,
            |_key, row| row.relation,
            |_key, dimension| dimension.relation,
        )
        .map_values(|_key, (row, matches)| fold_matches(row, matches, 19));
    let (after_build, build) = measure_phase("build", before_build, started, 1);

    let started = Instant::now();
    let output = plan.materialize();
    let (after_materialize, materialize) = measure_phase("materialize", after_build, started, 1);

    let started = Instant::now();
    for generation in 1..=SINGLE_UPDATES {
        source.insert(0, updated_row(0, generation));
        black_box(output.get_value(&0));
    }
    let (after_updates, updates) =
        measure_phase("single_updates", after_materialize, started, SINGLE_UPDATES);

    let started = Instant::now();
    source.insert_many(
        (0..BATCH_SIZE)
            .map(|key| (key, updated_row(key, SINGLE_UPDATES.wrapping_add(1))))
            .collect(),
    );
    black_box(output.get_value(&0));
    let (_, batch) = measure_phase("batch_100", after_updates, started, 1);

    let output_rows = output.snapshot().len();
    let before_teardown = Snapshot::now();
    let started = Instant::now();
    drop(output);
    let (_, teardown) = measure_phase("teardown", before_teardown, started, 1);

    print_measurements(
        revision,
        "two_join_region",
        &[build, materialize, updates, batch, teardown],
        output_rows,
    );
}

fn measure_four_join(revision: &str) {
    let source = source_rows();
    let shared = dimensions(23);
    let before_build = Snapshot::now();
    let started = Instant::now();
    let plan = source
        .clone()
        .left_join_by(
            shared.clone(),
            |_key, row| row.relation,
            |_key, dimension| dimension.relation,
        )
        .map_values(|_key, (row, matches)| fold_matches(row, matches, 1))
        .left_join_by(
            shared.clone(),
            |_key, row| row.relation,
            |_key, dimension| dimension.relation,
        )
        .map_values(|_key, (row, matches)| fold_matches(row, matches, 2))
        .left_join_by(
            shared.clone(),
            |_key, row| row.relation,
            |_key, dimension| dimension.relation,
        )
        .map_values(|_key, (row, matches)| fold_matches(row, matches, 3))
        .left_join_by(
            shared,
            |_key, row| row.relation,
            |_key, dimension| dimension.relation,
        )
        .map_values(|_key, (row, matches)| fold_matches(row, matches, 4));
    let (after_build, build) = measure_phase("build", before_build, started, 1);

    let started = Instant::now();
    let output = plan.materialize();
    let (after_materialize, materialize) = measure_phase("materialize", after_build, started, 1);

    let started = Instant::now();
    for generation in 1..=SINGLE_UPDATES {
        source.insert(0, updated_row(0, generation));
        black_box(output.get_value(&0));
    }
    let (after_updates, updates) =
        measure_phase("single_updates", after_materialize, started, SINGLE_UPDATES);

    let started = Instant::now();
    source.insert_many(
        (0..BATCH_SIZE)
            .map(|key| (key, updated_row(key, SINGLE_UPDATES.wrapping_add(1))))
            .collect(),
    );
    black_box(output.get_value(&0));
    let (_, batch) = measure_phase("batch_100", after_updates, started, 1);

    let output_rows = output.snapshot().len();
    let before_teardown = Snapshot::now();
    let started = Instant::now();
    drop(output);
    let (_, teardown) = measure_phase("teardown", before_teardown, started, 1);

    print_measurements(
        revision,
        "repeated_relation_four_join",
        &[build, materialize, updates, batch, teardown],
        output_rows,
    );
}

fn main() {
    let revision = option_env!("HYPHAE_BENCH_REVISION").unwrap_or("unknown");
    println!(
        "MAP_QUERY_ALLOCATION_CSV revision,scenario,phase,alloc_calls,alloc_bytes,dealloc_calls,dealloc_bytes,net_bytes,elapsed_ns,operations,output_rows"
    );
    measure_projection(revision);
    measure_two_join(revision);
    measure_four_join(revision);
}
