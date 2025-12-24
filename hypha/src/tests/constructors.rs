use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    thread,
    time::Duration,
};

use crate::{Signal, from_iter_with_delay, interval, traits::Watchable};

#[test]
fn test_interval_emits_incrementing() {
    let ticker = interval(Duration::from_millis(50));
    let received = Arc::new(AtomicU64::new(0));

    let r = received.clone();
    let _guard = ticker.subscribe(move |signal| {
        if let Signal::Value(v) = signal {
            r.store(**v, Ordering::SeqCst);
        }
    });

    assert_eq!(received.load(Ordering::SeqCst), 0);

    thread::sleep(Duration::from_millis(120));
    let val = received.load(Ordering::SeqCst);
    assert!(val >= 2, "expected at least 2 ticks, got {}", val);
}

#[test]
fn test_from_iter_with_delay_emits_all() {
    let items = from_iter_with_delay(vec![1u64, 2, 3], Duration::from_millis(30)).unwrap();
    let received = Arc::new(AtomicU64::new(0));

    let r = received.clone();
    let _guard = items.subscribe(move |signal| {
        if let Signal::Value(v) = signal {
            r.store(**v, Ordering::SeqCst);
        }
    });

    // Initial value
    assert_eq!(received.load(Ordering::SeqCst), 1);

    // Wait for all to emit
    thread::sleep(Duration::from_millis(100));
    assert_eq!(received.load(Ordering::SeqCst), 3);
}

#[test]
fn test_from_iter_with_delay_empty() {
    let items: Option<_> = from_iter_with_delay(Vec::<u64>::new(), Duration::from_millis(30));
    assert!(items.is_none());
}
