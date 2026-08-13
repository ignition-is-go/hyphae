//! Frozen near-threshold calibration for the four-stage left join region.

#![allow(
    clippy::arithmetic_side_effects,
    clippy::expect_used,
    clippy::panic,
    clippy::too_many_lines
)]

use std::time::{Duration, Instant};

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use hyphae::region_calibration::Snapshot;

#[allow(dead_code)]
#[path = "support/four_join_application_workload.rs"]
mod workload;

#[derive(Clone, Copy)]
enum StartState {
    Inactive,
    Active,
}

fn expected_delta(workers: usize, state: StartState, rows: u64) -> Snapshot {
    if workers <= 1 {
        return Snapshot {
            left_serial_dispatches: 1,
            ..Snapshot::default()
        };
    }
    match state {
        StartState::Inactive if rows >= 2_062 => Snapshot {
            left_parallel_dispatches: 1,
            inactive_to_parallel: 1,
            ..Snapshot::default()
        },
        StartState::Inactive => Snapshot {
            left_serial_dispatches: 1,
            ..Snapshot::default()
        },
        StartState::Active if rows < 990 => Snapshot {
            left_serial_dispatches: 1,
            parallel_to_inactive: 1,
            ..Snapshot::default()
        },
        StartState::Active => Snapshot {
            left_parallel_dispatches: 1,
            ..Snapshot::default()
        },
    }
}

fn prepare(fixture: &workload::ApplicationFixture, generation: &mut u64, state: StartState) {
    *generation = generation.saturating_add(1);
    fixture
        .source
        .insert_many(workload::calibration_update_batch(
            *generation,
            false,
            2_062,
        ));
    if matches!(state, StartState::Inactive) {
        *generation = generation.saturating_add(1);
        fixture
            .source
            .insert_many(workload::calibration_update_batch(*generation, false, 989));
    }
}

fn configured_workers() -> usize {
    std::env::var("HYPHAE_WORKER_THREADS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(4)
}

fn bench_calibration(c: &mut Criterion) {
    let workers = configured_workers();
    let states: &[StartState] = if workers <= 1 {
        &[StartState::Inactive]
    } else {
        &[StartState::Inactive, StartState::Active]
    };
    let mut group = c.benchmark_group("region_threshold_calibration");
    group.sample_size(50);
    for &state in states {
        let state_name = match state {
            StartState::Inactive => "inactive",
            StartState::Active => "active",
        };
        for rows in workload::REGION_CALIBRATION_ROWS {
            let fixture = workload::build_fixture();
            let mut generation = 0_u64;
            group.bench_with_input(
                BenchmarkId::new(state_name, rows),
                &rows,
                |b, &batch_rows| {
                    b.iter_custom(|iterations| {
                        let mut total = Duration::ZERO;
                        for _ in 0..iterations {
                            prepare(&fixture, &mut generation, state);
                            generation = generation.saturating_add(1);
                            let updates = workload::calibration_update_batch(
                                generation,
                                false,
                                batch_rows,
                            );
                            let sentinel = updates.first().expect("nonempty frozen batch").0;
                            let before = hyphae::region_calibration::snapshot();
                            let started = Instant::now();
                            fixture.source.insert_many(updates);
                            total = total.saturating_add(started.elapsed());
                            let delta = hyphae::region_calibration::snapshot().since(before);
                            assert_eq!(
                                delta,
                                expected_delta(workers, state, batch_rows),
                                "wrong branch for workers={workers}, state={state_name}, rows={batch_rows}"
                            );
                            let settled = fixture
                                .output
                                .get_value(&sentinel)
                                .expect("sentinel output must remain present");
                            assert_eq!(settled.generation, generation, "sentinel did not settle");
                            assert_eq!(settled.stage_mask, workload::EXPECTED_STAGE_MASK);
                        }
                        total
                    });
                },
            );
        }
    }
    group.finish();
}

criterion_group!(benches, bench_calibration);
criterion_main!(benches);
