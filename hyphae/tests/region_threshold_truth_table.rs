#![allow(clippy::arithmetic_side_effects, clippy::expect_used, clippy::panic)]

use hyphae::region_calibration::Snapshot;

#[allow(dead_code)]
#[path = "../benches/support/four_join_application_workload.rs"]
mod workload;

fn apply(fixture: &workload::ApplicationFixture, generation: &mut u64, rows: u64) {
    *generation = generation.saturating_add(1);
    fixture
        .source
        .insert_many(workload::calibration_update_batch(*generation, false, rows));
}

fn assert_case(
    fixture: &workload::ApplicationFixture,
    generation: &mut u64,
    initially_active: bool,
    rows: u64,
    expected: Snapshot,
) {
    apply(fixture, generation, 2_062);
    if !initially_active {
        apply(fixture, generation, 989);
    }
    let updates = workload::calibration_update_batch(generation.saturating_add(1), false, rows);
    let sentinel = updates.first().expect("nonempty frozen batch").0;
    *generation = generation.saturating_add(1);
    let before = hyphae::region_calibration::snapshot();
    fixture.source.insert_many(updates);
    assert_eq!(
        hyphae::region_calibration::snapshot().since(before),
        expected
    );
    assert_eq!(
        fixture
            .output
            .get_value(&sentinel)
            .expect("sentinel output")
            .generation,
        *generation
    );
}

#[test]
fn frozen_cost_brackets_and_dispatch_truth_table() {
    // JNil costs one unit and each of four stages adds 24: 97 per row.
    assert_eq!(989_usize * 97, 95_933);
    assert_eq!(990_usize * 97, 96_030);
    assert_eq!(991_usize * 97, 96_127);
    assert_eq!(2_061_usize * 97, 199_917);
    assert_eq!(2_062_usize * 97, 200_014);
    assert_eq!(2_063_usize * 97, 200_111);

    let fixture = workload::build_fixture();
    let mut generation = 0;
    for rows in [989, 990, 991, 2_061] {
        assert_case(
            &fixture,
            &mut generation,
            false,
            rows,
            Snapshot {
                left_serial_dispatches: 1,
                ..Snapshot::default()
            },
        );
    }
    for rows in [2_062, 2_063] {
        assert_case(
            &fixture,
            &mut generation,
            false,
            rows,
            Snapshot {
                left_parallel_dispatches: 1,
                inactive_to_parallel: 1,
                ..Snapshot::default()
            },
        );
    }
    assert_case(
        &fixture,
        &mut generation,
        true,
        989,
        Snapshot {
            left_serial_dispatches: 1,
            parallel_to_inactive: 1,
            ..Snapshot::default()
        },
    );
    for rows in [990, 991, 2_061, 2_062, 2_063] {
        assert_case(
            &fixture,
            &mut generation,
            true,
            rows,
            Snapshot {
                left_parallel_dispatches: 1,
                ..Snapshot::default()
            },
        );
    }
}
