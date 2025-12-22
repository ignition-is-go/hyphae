use crate::{Cell, Mutable, Watchable};
use std::thread;
use std::time::Duration;

#[test]
fn test_metrics_disabled_by_default() {
    let cell = Cell::new(0);
    assert!(cell.metrics().is_none());
}

#[test]
fn test_metrics_enabled_with_constructor() {
    let cell = Cell::with_metrics(0);
    assert!(cell.metrics().is_some());
}

#[test]
fn test_notify_count() {
    let cell = Cell::with_metrics(0);
    let metrics = cell.metrics().unwrap();

    assert_eq!(metrics.notify_count(), 0);

    cell.set(1);
    assert_eq!(metrics.notify_count(), 1);

    cell.set(2);
    cell.set(3);
    assert_eq!(metrics.notify_count(), 3);
}

#[test]
fn test_subscriber_count() {
    let cell = Cell::with_metrics(0);
    let metrics = cell.metrics().unwrap();

    assert_eq!(metrics.subscriber_count(), 0);

    let guard1 = cell.subscribe(|_| {});
    assert_eq!(metrics.subscriber_count(), 1);

    let guard2 = cell.subscribe(|_| {});
    assert_eq!(metrics.subscriber_count(), 2);

    drop(guard1);
    assert_eq!(metrics.subscriber_count(), 1);

    drop(guard2);
    assert_eq!(metrics.subscriber_count(), 0);
}

#[test]
fn test_notify_timing() {
    let cell = Cell::with_metrics(0);
    let metrics = cell.metrics().unwrap();

    // Subscribe with a slow callback
    let _guard = cell.subscribe(|_| {
        thread::sleep(Duration::from_millis(1));
    });

    cell.set(1);

    // Should have recorded some time
    assert!(metrics.total_notify_time_ns() > 0);
    assert!(metrics.avg_notify_time_ns() > 0);
}

#[test]
fn test_slowest_subscriber_tracking() {
    let cell = Cell::with_metrics(0);
    let metrics = cell.metrics().unwrap();

    // Subscribe with a slow callback
    let _guard = cell.subscribe(|_| {
        thread::sleep(Duration::from_millis(5));
    });

    cell.set(1);

    // Should have recorded the slow subscriber (at least 5ms = 5_000_000ns)
    let slowest = metrics.slowest_subscriber_ns();
    assert!(slowest >= 4_000_000, "Expected at least 4ms, got {}ns", slowest);
}

#[test]
fn test_reset_timing() {
    let cell = Cell::with_metrics(0);
    let metrics = cell.metrics().unwrap();

    let _guard = cell.subscribe(|_| {});

    cell.set(1);
    cell.set(2);

    assert!(metrics.notify_count() > 0);
    assert!(metrics.total_notify_time_ns() > 0);

    metrics.reset_timing();

    assert_eq!(metrics.notify_count(), 0);
    assert_eq!(metrics.total_notify_time_ns(), 0);
    assert_eq!(metrics.slowest_subscriber_ns(), 0);
    // subscriber_count should NOT be reset
    assert_eq!(metrics.subscriber_count(), 1);
}

#[test]
fn test_metrics_with_multiple_subscribers() {
    let cell = Cell::with_metrics(0);
    let metrics = cell.metrics().unwrap();

    let _guard1 = cell.subscribe(|_| {
        thread::sleep(Duration::from_micros(100));
    });
    let _guard2 = cell.subscribe(|_| {
        thread::sleep(Duration::from_millis(2));
    });
    let _guard3 = cell.subscribe(|_| {
        thread::sleep(Duration::from_micros(500));
    });

    cell.set(1);

    // The slowest subscriber should be the 2ms one
    let slowest = metrics.slowest_subscriber_ns();
    assert!(slowest >= 1_500_000, "Expected at least 1.5ms from slowest subscriber, got {}ns", slowest);
}
