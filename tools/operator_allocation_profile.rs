//! Allocation profile for v3 operator pipelines.
//!
//! This file is copied into the selected revision by
//! `bench-operator-allocations.sh`.

#![recursion_limit = "1024"]
#![type_length_limit = "16777216"]

use std::{
    alloc::{GlobalAlloc, Layout, System},
    hint::black_box,
    sync::atomic::{AtomicU64, Ordering},
    time::Instant,
};

use hyphae::{Cell, Gettable, JoinExt, MapExt, Materialize, Mutable};
use seq_macro::seq;

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
            ALLOC_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        }
        pointer
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc_zeroed(layout) };
        if !pointer.is_null() {
            ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
            ALLOC_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        DEALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
        DEALLOC_BYTES.fetch_add(layout.size() as u64, Ordering::Relaxed);
        unsafe { System.dealloc(pointer, layout) };
    }

    unsafe fn realloc(&self, pointer: *mut u8, old: Layout, new_size: usize) -> *mut u8 {
        let replacement = unsafe { System.realloc(pointer, old, new_size) };
        if !replacement.is_null() {
            DEALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
            DEALLOC_BYTES.fetch_add(old.size() as u64, Ordering::Relaxed);
            ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
            ALLOC_BYTES.fetch_add(new_size as u64, Ordering::Relaxed);
        }
        replacement
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

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
            alloc_calls: after.alloc_calls - before.alloc_calls,
            alloc_bytes: after.alloc_bytes - before.alloc_bytes,
            dealloc_calls: after.dealloc_calls - before.dealloc_calls,
            dealloc_bytes: after.dealloc_bytes - before.dealloc_bytes,
            elapsed_ns,
            operations,
        }
    }

    fn setup(build: Self, materialize: Self) -> Self {
        Self {
            phase: "total_setup",
            alloc_calls: build.alloc_calls + materialize.alloc_calls,
            alloc_bytes: build.alloc_bytes + materialize.alloc_bytes,
            dealloc_calls: build.dealloc_calls + materialize.dealloc_calls,
            dealloc_bytes: build.dealloc_bytes + materialize.dealloc_bytes,
            elapsed_ns: build.elapsed_ns + materialize.elapsed_ns,
            operations: 1,
        }
    }

    fn net_bytes(self) -> i128 {
        self.alloc_bytes as i128 - self.dealloc_bytes as i128
    }
}

fn combine((left, right): &(u64, u64)) -> u64 {
    left.wrapping_add(*right).rotate_left(3)
}

fn transform_1(value: &u64) -> u64 {
    value.wrapping_mul(0x9e37_79b9).rotate_left(7)
}

fn transform_2(value: &u64) -> u64 {
    value.rotate_right(11) ^ 0xa076_1d64_78bd_642f
}

fn transform_3(value: &u64) -> u64 {
    value.wrapping_mul(33).wrapping_add(17)
}

macro_rules! join_stage {
    ($plan:ident, $sources:ident, $index:literal) => {
        let $plan = $plan
            .join($sources[$index].clone())
            .map(combine as fn(&(u64, u64)) -> u64)
            .map(transform_1 as fn(&u64) -> u64)
            .map(transform_2 as fn(&u64) -> u64)
            .map(transform_3 as fn(&u64) -> u64);
    };
}

macro_rules! define_measurement {
    ($name:ident, $depth:literal) => {
        fn $name() -> [Measurement; 5] {
            const UPDATE_COUNT: u64 = 100;

            // Source setup and the result buffer are deliberately outside all
            // measured intervals.
            let root = Cell::new(1_u64);
            let sources: Vec<_> = (0..$depth)
                .map(|index| Cell::new(index as u64 + 10))
                .collect();

            let before_build = Snapshot::now();
            let build_started = Instant::now();
            let plan = root.clone();
            seq!(N in 0..$depth {
                join_stage!(plan, sources, N);
            });
            let build_elapsed = build_started.elapsed().as_nanos();
            let after_build = Snapshot::now();

            // Keep construction and the single v3 observation boundary as
            // separate measurements so future releases can identify which
            // phase changed.
            let materialize_started = Instant::now();
            let graph = plan.materialize();
            let materialize_elapsed = materialize_started.elapsed().as_nanos();
            let after_materialize = Snapshot::now();

            let update_started = Instant::now();
            for tick in 0..UPDATE_COUNT {
                let index = tick as usize % sources.len();
                sources[index].set(tick.wrapping_mul(101));
                black_box(graph.get());
            }
            let update_elapsed = update_started.elapsed().as_nanos();
            let after_updates = Snapshot::now();

            let teardown_started = Instant::now();
            drop(graph);
            let teardown_elapsed = teardown_started.elapsed().as_nanos();
            let after_teardown = Snapshot::now();

            let build = Measurement::between(
                "graph_build",
                before_build,
                after_build,
                build_elapsed,
                1,
            );
            let materialize = Measurement::between(
                "materialize",
                after_build,
                after_materialize,
                materialize_elapsed,
                1,
            );
            [
                build,
                materialize,
                Measurement::setup(build, materialize),
                Measurement::between(
                    "updates",
                    after_materialize,
                    after_updates,
                    update_elapsed,
                    UPDATE_COUNT,
                ),
                Measurement::between(
                    "teardown",
                    after_updates,
                    after_teardown,
                    teardown_elapsed,
                    1,
                ),
            ]
        }
    };
}

define_measurement!(measure_depth_4, 4);
define_measurement!(measure_depth_8, 8);
define_measurement!(measure_depth_16, 16);

fn main() {
    let revision = option_env!("HYPHAE_BENCH_REVISION").unwrap_or("unknown");
    let cases = [
        (4_usize, measure_depth_4()),
        (8_usize, measure_depth_8()),
        (16_usize, measure_depth_16()),
    ];

    println!(
        "ALLOCATION_CSV revision,join_stages,operators,phase,alloc_calls,alloc_bytes,dealloc_calls,dealloc_bytes,net_bytes,elapsed_ns,operations"
    );
    for (depth, measurements) in cases {
        for measurement in measurements {
            println!(
                "ALLOCATION_CSV {revision},{depth},{},{},{},{},{},{},{},{},{}",
                depth * 5,
                measurement.phase,
                measurement.alloc_calls,
                measurement.alloc_bytes,
                measurement.dealloc_calls,
                measurement.dealloc_bytes,
                measurement.net_bytes(),
                measurement.elapsed_ns,
                measurement.operations,
            );
        }
    }
}
