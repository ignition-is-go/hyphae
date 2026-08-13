#[path = "../benches/support/four_join_application_workload.rs"]
mod workload;

#[test]
fn four_join_application_matches_reference_order_and_settles_immediately() {
    workload::assert_preflight();
}
