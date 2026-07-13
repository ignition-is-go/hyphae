//! Within-pass re-fire counter (`profiling` feature).
//!
//! Proves the measurement primitive that sizes the coalesceable fraction: on
//! the synchronous path a diamond re-fires its sink, and [`pass`] reports that
//! re-fire; wrap the same change in [`batch`](hyphae::batch) and the re-fire
//! collapses — the same metric, measured the same way, before and after.
#![cfg(feature = "profiling")]

use hyphae::{
    Cell, JoinExt, MapExt, MaterializeDefinite, Mutable, Watchable,
    profiling::{pass, take_report},
};

/// `tick_active()` (whether a `batch()` is open) is a process-wide flag under
/// the `scheduler` feature (see `hyphae::scheduler`'s cross-thread docs), so
/// the synchronous-path test below needs no OTHER test's `batch()` call open
/// concurrently on another thread — otherwise this test's diamond would take
/// the deferred/coalesced path instead of the synchronous one it's measuring.
/// Serialize this file's tests instead of relying on run order.
fn scheduler_test_serial() -> std::sync::MutexGuard<'static, ()> {
    // Force the wave-parallel drain path at test width: production defaults
    // the group threshold high (waves stay sequential at rest), so parallelism
    // tests must lower it to actually exercise concurrent same-height groups.
    hyphae::scheduler::set_wave_threshold_for_test(4);
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[test]
fn pass_reports_a_synchronous_diamond_refire() {
    let _serial = scheduler_test_serial();
    // s ─┬─> a = s + 1 ─┐
    //    └─> b = s * 10 ┴─> sink = a + b
    let s = Cell::new(0i64);
    let a = s.clone().map(|x| x + 1).materialize();
    let b = s.clone().map(|x| x * 10).materialize();
    let sink = a.join(&b).map(|(x, y)| x + y).materialize();
    let guard = sink.subscribe(|_| {});
    std::mem::forget(guard);

    // One source change reaches `sink` via both legs, so it fans out twice —
    // one coalesceable re-fire.
    pass(|| {
        s.set(5);
    });
    let report = take_report().expect("a pass just completed");

    assert!(
        report.total_refires() >= 1,
        "the diamond re-fires the sink; per_cell = {:?}",
        report.per_cell
    );
    assert!(
        !report.refiring_cells().is_empty(),
        "the re-firing sink should be reported"
    );
    assert!(
        report.coalesceable_fraction() > 0.0,
        "a re-fire means a non-zero coalesceable fraction"
    );
}

#[test]
fn take_report_is_consumed_by_the_read() {
    let _serial = scheduler_test_serial();
    pass(|| {});
    assert!(
        take_report().is_some(),
        "the completed pass sealed a report"
    );
    assert!(
        take_report().is_none(),
        "the report is cleared by the first take"
    );
}

#[test]
fn no_pass_no_report() {
    let _serial = scheduler_test_serial();
    // Fanouts outside any pass are not tallied.
    let s = Cell::new(0i64);
    let guard = s.clone().map(|x| x + 1).materialize().subscribe(|_| {});
    std::mem::forget(guard);
    let _ = take_report(); // drain any prior
    s.set(1);
    assert!(take_report().is_none(), "no pass was open, so no report");
}

#[cfg(feature = "scheduler")]
#[test]
fn batch_collapses_the_refire_the_pass_measures() {
    use hyphae::batch;

    let _serial = scheduler_test_serial();
    let s = Cell::new(0i64);
    let a = s.clone().map(|x| x + 1).materialize();
    let b = s.clone().map(|x| x * 10).materialize();
    let sink = a.join(&b).map(|(x, y)| x + y).materialize();
    let guard = sink.subscribe(|_| {});
    std::mem::forget(guard);

    // Same graph, same source change — but coalesced. Every cell fans out at
    // most once, so the re-fire the synchronous pass saw is gone. This is the
    // A/B: identical metric, before (>=1) and after (0).
    pass(|| batch(|| s.set(5)));
    let report = take_report().expect("a pass just completed");

    assert_eq!(
        report.total_refires(),
        0,
        "batch coalesces every cell to one fanout; per_cell = {:?}",
        report.per_cell
    );
}
