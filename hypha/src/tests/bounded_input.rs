use crate::{BoundedInput, Gettable, OverflowPolicy, Signal, Watchable};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

#[test]
fn test_basic_push_and_get() {
    let input = BoundedInput::new(0, 10, OverflowPolicy::DropNewest);

    input.push(1).unwrap();
    // get() flushes the buffer first
    assert_eq!(input.get(), 1);

    input.push(2).unwrap();
    assert_eq!(input.get(), 2);
}

#[test]
fn test_subscribe_receives_values() {
    let input = BoundedInput::new(0, 10, OverflowPolicy::DropNewest);
    let received = Arc::new(AtomicUsize::new(0));
    let received_clone = received.clone();

    let _guard = input.subscribe(move |signal| {
        if let Signal::Value(v) = signal {
            received_clone.store(**v, Ordering::SeqCst);
        }
    });

    // Use push_flush to notify subscribers immediately
    input.push_flush(42).unwrap();
    assert_eq!(received.load(Ordering::SeqCst), 42);

    input.push_flush(100).unwrap();
    assert_eq!(received.load(Ordering::SeqCst), 100);
}

#[test]
fn test_drop_newest_policy() {
    let input = BoundedInput::new(0, 2, OverflowPolicy::DropNewest);

    // Fill the buffer (values stay buffered, not drained immediately)
    input.push(1).unwrap();
    input.push(2).unwrap();

    // This should drop the incoming value (3) and return Ok
    let result = input.push(3);
    assert!(result.is_ok());

    // Metrics should show 1 drop
    assert_eq!(input.metrics().dropped_count(), 1);
    assert_eq!(input.metrics().backpressure_events(), 1);

    // Flush and get - should have 1 and 2 (3 was dropped)
    // get() flushes, so we'll get 2 (the last value flushed)
    assert_eq!(input.get(), 2);
}

#[test]
fn test_drop_oldest_policy() {
    let input = BoundedInput::new(0, 2, OverflowPolicy::DropOldest);

    // Fill the buffer
    input.push(1).unwrap();
    input.push(2).unwrap();

    // This should drop the oldest (1) and push 3
    input.push(3).unwrap();

    assert_eq!(input.metrics().dropped_count(), 1);
    assert_eq!(input.metrics().backpressure_events(), 1);

    // Buffer now has [2, 3], get() flushes and returns last value
    assert_eq!(input.get(), 3);
}

#[test]
fn test_error_policy() {
    let input = BoundedInput::new(0, 1, OverflowPolicy::Error);

    // Fill the buffer
    input.push(1).unwrap();

    // This should fail and error the cell
    let result = input.push(2);
    assert!(result.is_err());
    assert!(input.is_error());
    assert_eq!(input.metrics().backpressure_events(), 1);
}

#[test]
fn test_close() {
    let input = BoundedInput::new(0, 10, OverflowPolicy::DropNewest);

    input.push(1).unwrap();
    assert!(!input.is_closed());
    assert!(!input.is_complete());

    input.close();

    assert!(input.is_closed());
    assert!(input.is_complete());

    // Push after close should fail
    let result = input.push(2);
    assert!(result.is_err());
}

#[test]
fn test_metrics_tracking() {
    let input = BoundedInput::new(0, 5, OverflowPolicy::DropNewest);

    for i in 1..=10 {
        let _ = input.push(i);
    }

    // First 5 succeed, remaining 5 are dropped (buffer stays full)
    assert_eq!(input.metrics().total_pushed(), 5);
    assert_eq!(input.metrics().dropped_count(), 5);
    assert_eq!(input.metrics().backpressure_events(), 5);
}

#[test]
fn test_capacity() {
    let input = BoundedInput::new(0, 42, OverflowPolicy::DropNewest);
    assert_eq!(input.capacity(), 42);
}

#[test]
fn test_to_cell() {
    let input = BoundedInput::new(0, 10, OverflowPolicy::DropNewest);

    // push_flush to immediately forward to cell
    input.push_flush(5).unwrap();

    let cell = input.to_cell();
    assert_eq!(cell.get(), 5);

    // Original input can still push and flush
    input.push_flush(10).unwrap();

    // Cell sees the update
    assert_eq!(cell.get(), 10);
}

#[test]
fn test_clone() {
    let input1 = BoundedInput::new(0, 10, OverflowPolicy::DropNewest);
    let input2 = input1.clone();

    input1.push_flush(42).unwrap();

    // Both should see the same value (they share the same inner cell)
    assert_eq!(input1.get(), 42);
    assert_eq!(input2.get(), 42);
}

#[test]
fn test_complete_signal_on_close() {
    let input = BoundedInput::new(0, 10, OverflowPolicy::DropNewest);
    let completed = Arc::new(AtomicUsize::new(0));
    let completed_clone = completed.clone();

    let _guard = input.subscribe(move |signal| {
        if let Signal::Complete = signal {
            completed_clone.store(1, Ordering::SeqCst);
        }
    });

    assert_eq!(completed.load(Ordering::SeqCst), 0);

    input.close();

    assert_eq!(completed.load(Ordering::SeqCst), 1);
}

#[test]
#[should_panic(expected = "capacity must be positive")]
fn test_zero_capacity_panics() {
    let _ = BoundedInput::new(0, 0, OverflowPolicy::DropNewest);
}
