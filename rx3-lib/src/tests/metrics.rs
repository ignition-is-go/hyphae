use crate::{Cell, Gettable, Mutable, Watchable};
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
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

#[test]
fn test_last_notify_time() {
    let cell = Cell::with_metrics(0);
    let metrics = cell.metrics().unwrap();

    assert_eq!(metrics.last_notify_time_ns(), 0);

    let _guard = cell.subscribe(|_| {
        thread::sleep(Duration::from_millis(5));
    });
    cell.set(1);

    let last = metrics.last_notify_time_ns();
    assert!(last >= 4_000_000, "Expected at least 4ms, got {}ns", last);
}

#[test]
fn test_is_backed_up_false_without_metrics() {
    let cell = Cell::new(0);
    // Without metrics, is_backed_up always returns false
    assert!(!cell.is_backed_up());
}

#[test]
fn test_is_backed_up_false_initially() {
    let cell = Cell::with_metrics(0);
    // No notify yet, so not backed up
    assert!(!cell.is_backed_up());
}

#[test]
fn test_is_backed_up_with_slow_subscriber() {
    let cell = Cell::with_metrics(0);

    let _guard = cell.subscribe(|_| {
        thread::sleep(Duration::from_millis(5));
    });

    // First set - triggers slow notify
    cell.set(1);

    // 5ms > 1ms default threshold, so should be backed up
    assert!(cell.is_backed_up());
}

#[test]
fn test_is_backed_up_threshold_custom() {
    let cell = Cell::with_metrics(0);

    let _guard = cell.subscribe(|_| {
        thread::sleep(Duration::from_millis(5));
    });

    cell.set(1);

    // 5ms < 10ms threshold, so not backed up
    assert!(!cell.is_backed_up_threshold(Duration::from_millis(10)));

    // 5ms > 2ms threshold, so backed up
    assert!(cell.is_backed_up_threshold(Duration::from_millis(2)));
}

#[test]
fn test_try_set_succeeds_when_not_backed_up() {
    let cell = Cell::with_metrics(0);

    // No previous notify, so try_set should succeed
    assert!(cell.try_set(1).is_ok());
    assert_eq!(cell.get(), 1);
}

#[test]
fn test_try_set_fails_when_backed_up() {
    let cell = Cell::with_metrics(0);

    let _guard = cell.subscribe(|_| {
        thread::sleep(Duration::from_millis(5));
    });

    // First set succeeds (no previous slow notify)
    assert!(cell.try_set(1).is_ok());

    // Second try_set fails because previous notify was slow (5ms > 1ms)
    let result = cell.try_set(2);
    assert!(result.is_err());
    assert_eq!(result.unwrap_err(), 2); // Value returned on error

    // Cell still has old value
    assert_eq!(cell.get(), 1);
}

#[test]
fn test_try_set_threshold() {
    let cell = Cell::with_metrics(0);

    let _guard = cell.subscribe(|_| {
        thread::sleep(Duration::from_millis(5));
    });

    cell.set(1);

    // 5ms < 10ms threshold, so try_set_threshold should succeed
    assert!(cell.try_set_threshold(2, Duration::from_millis(10)).is_ok());
    assert_eq!(cell.get(), 2);

    // Now 5ms > 2ms threshold, so should fail
    assert!(cell.try_set_threshold(3, Duration::from_millis(2)).is_err());
}

#[test]
fn test_slow_subscriber_callback() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let cell = Cell::with_metrics(0);

    let alert_count = Arc::new(AtomicUsize::new(0));
    let last_duration = Arc::new(AtomicU64::new(0));

    let ac = alert_count.clone();
    let ld = last_duration.clone();
    cell.on_slow_subscriber(Duration::from_millis(2), move |alert| {
        ac.fetch_add(1, Ordering::SeqCst);
        ld.store(alert.duration_ns, Ordering::SeqCst);
    });

    // Add a slow subscriber
    let _guard = cell.subscribe(|_| {
        thread::sleep(Duration::from_millis(5));
    });

    // First set triggers the slow subscriber
    cell.set(1);

    // Alert should have been triggered (5ms > 2ms threshold)
    assert_eq!(alert_count.load(Ordering::SeqCst), 1);
    assert!(last_duration.load(Ordering::SeqCst) >= 4_000_000); // At least 4ms in ns
}

#[test]
fn test_slow_subscriber_callback_not_triggered_for_fast() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let cell = Cell::with_metrics(0);

    let alert_count = Arc::new(AtomicUsize::new(0));

    let ac = alert_count.clone();
    cell.on_slow_subscriber(Duration::from_millis(100), move |_alert| {
        ac.fetch_add(1, Ordering::SeqCst);
    });

    // Add a fast subscriber
    let _guard = cell.subscribe(|_| {
        // Fast - no sleep
    });

    cell.set(1);
    cell.set(2);
    cell.set(3);

    // No alerts - all subscribers were fast
    assert_eq!(alert_count.load(Ordering::SeqCst), 0);
}
