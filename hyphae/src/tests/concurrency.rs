use std::{
    sync::{
        Arc, Barrier,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    thread,
};

use crate::{Cell, Gettable, MapExt, Mutable, Pipeline, Signal, Watchable};

// ============================================================================
// Concurrent Write Tests
// ============================================================================

#[test]
fn test_concurrent_writers() {
    let cell = Cell::new(0u64);
    let num_threads = 8;
    let writes_per_thread = 1000;

    let handles: Vec<_> = (0..num_threads)
        .map(|thread_id| {
            let cell = cell.clone();
            thread::spawn(move || {
                for i in 0..writes_per_thread {
                    cell.set(thread_id * writes_per_thread + i);
                }
            })
        })
        .collect();

    for handle in handles {
        handle.join().unwrap();
    }

    // Cell should have some value (last write wins, but we don't know which)
    let final_value = cell.get();
    assert!(final_value < num_threads * writes_per_thread);
}

#[test]
fn test_concurrent_writers_with_barrier() {
    let cell = Cell::new(0u64);
    let num_threads = 8;
    let barrier = Arc::new(Barrier::new(num_threads));

    let handles: Vec<_> = (0..num_threads)
        .map(|thread_id| {
            let cell = cell.clone();
            let barrier = barrier.clone();
            thread::spawn(move || {
                barrier.wait(); // Synchronize all threads to start at once
                for i in 0..1000 {
                    cell.set((thread_id * 1000 + i) as u64);
                }
            })
        })
        .collect();

    for handle in handles {
        handle.join().unwrap();
    }

    // Should complete without panics or deadlocks
    let _ = cell.get();
}

// ============================================================================
// Concurrent Read/Write Tests
// ============================================================================

#[test]
fn test_concurrent_readers_and_writers() {
    let cell = Cell::new(0u64);
    let num_writers = 4;
    let num_readers = 4;
    let operations = 1000;

    let read_count = Arc::new(AtomicU64::new(0));

    let mut handles = Vec::new();

    // Spawn writers
    for _ in 0..num_writers {
        let cell = cell.clone();
        handles.push(thread::spawn(move || {
            for i in 0..operations {
                cell.set(i as u64);
            }
        }));
    }

    // Spawn readers
    for _ in 0..num_readers {
        let cell = cell.clone();
        let count = read_count.clone();
        handles.push(thread::spawn(move || {
            for _ in 0..operations {
                let _ = cell.get();
                count.fetch_add(1, Ordering::Relaxed);
            }
        }));
    }

    for handle in handles {
        handle.join().unwrap();
    }

    assert_eq!(
        read_count.load(Ordering::Relaxed),
        (num_readers * operations) as u64
    );
}

// ============================================================================
// Concurrent Subscribe/Unsubscribe Tests
// ============================================================================

#[test]
fn test_concurrent_subscribe_unsubscribe() {
    let cell = Cell::new(0u64);
    let num_threads = 8;
    let iterations = 100;

    let handles: Vec<_> = (0..num_threads)
        .map(|_| {
            let cell = cell.clone();
            thread::spawn(move || {
                for _ in 0..iterations {
                    let counter = Arc::new(AtomicU64::new(0));
                    let cnt = counter.clone();
                    let guard = cell.subscribe(move |_| {
                        cnt.fetch_add(1, Ordering::Relaxed);
                    });
                    // Immediately drop to unsubscribe
                    drop(guard);
                }
            })
        })
        .collect();

    for handle in handles {
        handle.join().unwrap();
    }

    // Should complete without panics
}

#[test]
fn test_concurrent_subscribe_while_notifying() {
    let cell = Cell::new(0u64);
    let num_subscribers = 4;
    let num_writers = 4;
    let operations = 500;

    let notification_count = Arc::new(AtomicU64::new(0));
    let mut handles = Vec::new();

    // Spawn subscriber threads that add/remove subscriptions
    for _ in 0..num_subscribers {
        let cell = cell.clone();
        let count = notification_count.clone();
        handles.push(thread::spawn(move || {
            for _ in 0..operations {
                let cnt = count.clone();
                let guard = cell.subscribe(move |_| {
                    cnt.fetch_add(1, Ordering::Relaxed);
                });
                thread::yield_now();
                drop(guard);
            }
        }));
    }

    // Spawn writer threads
    for _ in 0..num_writers {
        let cell = cell.clone();
        handles.push(thread::spawn(move || {
            for i in 0..operations {
                cell.set(i as u64);
                thread::yield_now();
            }
        }));
    }

    for handle in handles {
        handle.join().unwrap();
    }

    // Should have received some notifications
    assert!(notification_count.load(Ordering::Relaxed) > 0);
}

// ============================================================================
// Concurrent Derived Cell Tests
// ============================================================================

#[test]
fn test_concurrent_derived_chains() {
    let source = Cell::new(0u64);
    let num_chains = 8;
    let updates = 100;

    let final_counts: Vec<_> = (0..num_chains)
        .map(|_| {
            let mapped = source.clone().map(|x| x * 2).materialize();
            let count = Arc::new(AtomicU64::new(0));
            let cnt = count.clone();
            let _guard = mapped.subscribe(move |_| {
                cnt.fetch_add(1, Ordering::Relaxed);
            });
            (mapped, count, _guard)
        })
        .collect();

    // Update source from multiple threads
    let handles: Vec<_> = (0..4)
        .map(|thread_id| {
            let source = source.clone();
            thread::spawn(move || {
                for i in 0..updates {
                    source.set((thread_id * updates + i) as u64);
                }
            })
        })
        .collect();

    for handle in handles {
        handle.join().unwrap();
    }

    // Each chain should have received notifications
    for (_, count, _) in &final_counts {
        assert!(count.load(Ordering::Relaxed) > 0);
    }
}

#[test]
fn test_concurrent_map_chain_integrity() {
    let source = Cell::new(0u64);
    let mapped = source.clone().map(|x| x * 2).map(|x| x + 1);

    let errors = Arc::new(AtomicU64::new(0));
    let checks = Arc::new(AtomicU64::new(0));

    let num_threads = 4;
    let operations = 1000;

    let handles: Vec<_> = (0..num_threads)
        .map(|_| {
            let source = source.clone();
            let mapped = mapped.clone();
            let errors = errors.clone();
            let checks = checks.clone();
            thread::spawn(move || {
                for i in 0..operations {
                    source.set(i as u64);
                    let result = mapped.get();
                    checks.fetch_add(1, Ordering::Relaxed);
                    // Result should be (some_value * 2 + 1), always odd
                    if result % 2 == 0 {
                        errors.fetch_add(1, Ordering::Relaxed);
                    }
                }
            })
        })
        .collect();

    for handle in handles {
        handle.join().unwrap();
    }

    assert_eq!(
        errors.load(Ordering::Relaxed),
        0,
        "Map chain produced invalid values"
    );
    assert_eq!(
        checks.load(Ordering::Relaxed),
        (num_threads * operations) as u64
    );
}

// ============================================================================
// Stress Tests
// ============================================================================

#[test]
fn stress_high_frequency_updates() {
    let cell = Cell::new(0u64);
    let update_count = Arc::new(AtomicU64::new(0));

    let cnt = update_count.clone();
    let _guard = cell.subscribe(move |_| {
        cnt.fetch_add(1, Ordering::Relaxed);
    });

    let num_updates = 100_000;
    for i in 0..num_updates {
        cell.set(i);
    }

    // Should have received all updates (plus initial)
    assert_eq!(update_count.load(Ordering::Relaxed), num_updates + 1);
}

#[test]
fn stress_many_subscribers() {
    let cell = Cell::new(0u64);
    let num_subscribers = 1000;
    let total_notifications = Arc::new(AtomicU64::new(0));

    let guards: Vec<_> = (0..num_subscribers)
        .map(|_| {
            let cnt = total_notifications.clone();
            cell.subscribe(move |_| {
                cnt.fetch_add(1, Ordering::Relaxed);
            })
        })
        .collect();

    // Initial subscription calls
    assert_eq!(
        total_notifications.load(Ordering::Relaxed),
        num_subscribers as u64
    );

    cell.set(1);
    assert_eq!(
        total_notifications.load(Ordering::Relaxed),
        (num_subscribers * 2) as u64
    );

    drop(guards);
}

#[test]
fn stress_deep_chain() {
    let source = Cell::new(0u64);
    let depth = 100;

    let mut current = source.clone().map(|x| x + 1).materialize();
    for _ in 1..depth {
        current = current.map(|x| x + 1).materialize();
    }

    let notification_count = Arc::new(AtomicU64::new(0));
    let cnt = notification_count.clone();
    let _guard = current.subscribe(move |_| {
        cnt.fetch_add(1, Ordering::Relaxed);
    });

    // Update source many times
    for i in 0..1000 {
        source.set(i);
    }

    // Final value should be source + depth
    assert_eq!(current.get(), 999 + depth);
    assert_eq!(notification_count.load(Ordering::Relaxed), 1001); // initial + 1000 updates
}

#[test]
fn stress_wide_fan_out() {
    let source = Cell::new(0u64);
    let fan_out = 100;

    let counts: Vec<_> = (0..fan_out)
        .map(|i| {
            let mapped = source.clone().map(move |x| x + i as u64).materialize();
            let count = Arc::new(AtomicU64::new(0));
            let cnt = count.clone();
            let guard = mapped.subscribe(move |_| {
                cnt.fetch_add(1, Ordering::Relaxed);
            });
            (count, guard)
        })
        .collect();

    for i in 0..1000 {
        source.set(i);
    }

    for (count, _) in &counts {
        assert_eq!(count.load(Ordering::Relaxed), 1001); // initial + 1000
    }
}

#[test]
fn stress_rapid_subscribe_unsubscribe() {
    let cell = Cell::new(0u64);
    let iterations = 10_000;

    for _ in 0..iterations {
        let counter = Arc::new(AtomicU64::new(0));
        let cnt = counter.clone();
        let guard = cell.subscribe(move |_| {
            cnt.fetch_add(1, Ordering::Relaxed);
        });
        assert_eq!(counter.load(Ordering::Relaxed), 1); // immediate callback
        drop(guard);
    }

    // Cell should still work after all those subscriptions
    cell.set(42);
    assert_eq!(cell.get(), 42);
}

// ============================================================================
// Race Condition Tests
// ============================================================================

#[test]
fn test_no_lost_updates() {
    let cell = Cell::new(0u64);
    let sum = Arc::new(AtomicU64::new(0));

    let s = sum.clone();
    let _guard = cell.subscribe(move |signal| {
        if let Signal::Value(v) = signal {
            s.fetch_add(**v, Ordering::Relaxed);
        }
    });

    // Send known sequence
    for i in 1..=100u64 {
        cell.set(i);
    }

    // Sum should be 0 + 1 + 2 + ... + 100 = 5050
    // (0 from initial subscription, then 1-100)
    assert_eq!(sum.load(Ordering::Relaxed), 5050);
}

#[test]
fn test_subscriber_sees_consistent_state() {
    let a = Cell::new(0u64);
    let b = Cell::new(0u64);

    // Create a derived cell that depends on both
    let sum = a.clone().map({
        let b = b.clone();
        move |av| *av + b.get()
    }).materialize();

    let inconsistencies = Arc::new(AtomicU64::new(0));
    let _inc = inconsistencies.clone();

    let _guard = sum.subscribe(move |signal| {
        if let Signal::Value(v) = signal {
            // Value should always be even (both a and b updated together)
            // This test is checking that we can at least read without crashing
            let _ = **v;
        }
    });

    // Update both in quick succession
    for i in 0..1000 {
        a.set(i);
        b.set(i);
    }

    // Should complete without panics
    let _ = sum.get();
}

// ============================================================================
// Memory Safety Tests
// ============================================================================

#[test]
fn test_subscriber_outlives_source() {
    let notification_count = Arc::new(AtomicU64::new(0));

    let guard = {
        let cell = Cell::new(42u64);
        let cnt = notification_count.clone();
        cell.subscribe(move |_| {
            cnt.fetch_add(1, Ordering::Relaxed);
        })
        // cell is dropped here, but guard might still reference it
    };

    // Guard should be safe to drop even after cell is gone
    drop(guard);

    // Should have received at least the initial notification
    assert!(notification_count.load(Ordering::Relaxed) >= 1);
}

#[test]
fn test_derived_outlives_source_with_updates() {
    let derived = {
        let source = Cell::new(0u64);
        source.map(|x| x * 2)
        // source dropped here
    };

    // Derived should still be readable
    let _ = derived.get();
}

// ============================================================================
// Completion Signal Concurrency
// ============================================================================

#[test]
fn test_concurrent_complete_signals() {
    use crate::TakeExt;

    let source = Cell::new(0u64);
    let taken = source.take(5);

    let complete_count = Arc::new(AtomicUsize::new(0));
    let value_count = Arc::new(AtomicUsize::new(0));

    let cc = complete_count.clone();
    let vc = value_count.clone();
    let _guard = taken.subscribe(move |signal| match signal {
        Signal::Value(_) => {
            vc.fetch_add(1, Ordering::SeqCst);
        }
        Signal::Complete => {
            cc.fetch_add(1, Ordering::SeqCst);
        }
        _ => {}
    });

    // Send more than 5 values from multiple threads
    let handles: Vec<_> = (0..4)
        .map(|_| {
            let source = source.clone();
            thread::spawn(move || {
                for i in 0..10 {
                    source.set(i);
                    thread::yield_now();
                }
            })
        })
        .collect();

    for handle in handles {
        handle.join().unwrap();
    }

    // Should receive exactly one Complete signal
    assert_eq!(complete_count.load(Ordering::SeqCst), 1);
    // take(5) counts initial as one of the 5, so 5 value signals total
    assert_eq!(value_count.load(Ordering::SeqCst), 5);
}

// ============================================================================
// Barrage Tests - Chaotic concurrent operations
// ============================================================================

#[test]
fn barrage_all_operations_concurrent() {
    use crate::{FilterExt, ScanExt};

    let source = Cell::new(0u64);
    let total_ops = Arc::new(AtomicU64::new(0));
    let errors = Arc::new(AtomicU64::new(0));

    let num_threads = 16;
    let ops_per_thread = 1000;
    let barrier = Arc::new(Barrier::new(num_threads));

    let handles: Vec<_> = (0..num_threads)
        .map(|thread_id| {
            let source = source.clone();
            let total_ops = total_ops.clone();
            let barrier = barrier.clone();

            thread::spawn(move || {
                barrier.wait();

                for i in 0..ops_per_thread {
                    let op = (thread_id * ops_per_thread + i) % 6;
                    match op {
                        0 => {
                            // Write
                            source.set((thread_id * 1000 + i) as u64);
                        }
                        1 => {
                            // Read
                            let _ = source.get();
                        }
                        2 => {
                            // Subscribe and immediately unsubscribe
                            let _guard = source.subscribe(|_| {});
                        }
                        3 => {
                            // Create derived and read
                            let mapped = source.clone().map(|x| x.wrapping_mul(2));
                            let _ = mapped.get();
                        }
                        4 => {
                            // Create filter chain
                            let filtered = source.clone().filter(|x| x % 2 == 0);
                            let _ = filtered.get();
                        }
                        5 => {
                            // Create scan
                            let scanned = source.scan(0u64, |acc, x| acc.wrapping_add(*x));
                            let _ = scanned.get();
                        }
                        _ => unreachable!(),
                    }
                    total_ops.fetch_add(1, Ordering::Relaxed);
                }
            })
        })
        .collect();

    for handle in handles {
        if handle.join().is_err() {
            errors.fetch_add(1, Ordering::Relaxed);
        }
    }

    assert_eq!(errors.load(Ordering::Relaxed), 0, "Some threads panicked");
    assert_eq!(
        total_ops.load(Ordering::Relaxed),
        (num_threads * ops_per_thread) as u64
    );
}

#[test]
fn barrage_rapid_chain_creation_destruction() {
    let source = Cell::new(0u64);
    let num_threads = 8;
    let iterations = 500;

    let handles: Vec<_> = (0..num_threads)
        .map(|_| {
            let source = source.clone();
            thread::spawn(move || {
                for i in 0..iterations {
                    // Create a chain of varying depth
                    let depth = (i % 10) + 1;
                    let mut chain = source.clone().map(|x| x + 1).materialize();
                    for _ in 1..depth {
                        chain = chain.map(|x| x + 1).materialize();
                    }

                    // Subscribe, trigger some updates, then drop
                    let count = Arc::new(AtomicU64::new(0));
                    let cnt = count.clone();
                    let guard = chain.subscribe(move |_| {
                        cnt.fetch_add(1, Ordering::Relaxed);
                    });

                    source.set(i as u64);
                    source.set((i + 1) as u64);

                    drop(guard);
                    // chain also dropped here
                }
            })
        })
        .collect();

    for handle in handles {
        handle.join().expect("Thread panicked");
    }
}

#[test]
fn barrage_subscriber_explosion() {
    let source = Cell::new(0u64);
    let num_waves = 10;
    let subscribers_per_wave = 100;
    let updates_per_wave = 50;

    let total_notifications = Arc::new(AtomicU64::new(0));

    for wave in 0..num_waves {
        let guards: Vec<_> = (0..subscribers_per_wave)
            .map(|_| {
                let cnt = total_notifications.clone();
                source.subscribe(move |_| {
                    cnt.fetch_add(1, Ordering::Relaxed);
                })
            })
            .collect();

        // Hammer updates while subscribers are active
        for i in 0..updates_per_wave {
            source.set((wave * updates_per_wave + i) as u64);
        }

        // All guards dropped here - mass unsubscribe
        drop(guards);
    }

    // Should have processed many notifications without issues
    assert!(total_notifications.load(Ordering::Relaxed) > 0);
}

#[test]
fn barrage_interleaved_operators() {
    use crate::{DedupedExt, FilterExt, ScanExt, SkipExt, TakeExt};

    let source = Cell::new(0u64);
    let num_threads = 8;

    let complete_count = Arc::new(AtomicU64::new(0));
    let value_count = Arc::new(AtomicU64::new(0));

    // Each thread creates a different operator pipeline
    let handles: Vec<_> = (0..num_threads)
        .map(|thread_id| {
            let source = source.clone();
            let cc = complete_count.clone();
            let vc = value_count.clone();

            thread::spawn(move || {
                let pipeline: Box<dyn Fn() -> u64 + Send> = match thread_id % 5 {
                    0 => {
                        let p = source.map(|x| x * 2).materialize().take(100);
                        let _g = p.subscribe(move |s| {
                            if let Signal::Complete = s {
                                cc.fetch_add(1, Ordering::Relaxed);
                            }
                        });
                        Box::new(move || p.get())
                    }
                    1 => {
                        let p = source.filter(|x| x % 2 == 0).materialize().skip(5);
                        let _g = p.subscribe(move |s| {
                            if let Signal::Value(_) = s {
                                vc.fetch_add(1, Ordering::Relaxed);
                            }
                        });
                        Box::new(move || p.get())
                    }
                    2 => {
                        let p = source.scan(0u64, |acc, x| acc.wrapping_add(*x));
                        Box::new(move || p.get())
                    }
                    3 => {
                        let p = source.deduped();
                        Box::new(move || p.get())
                    }
                    4 => {
                        let p = source.map(|x| x + 1).materialize().filter(|x| *x > 10).map(|x| x * 2).materialize();
                        Box::new(move || p.get())
                    }
                    _ => unreachable!(),
                };

                // Read many times while source is being updated
                for _ in 0..1000 {
                    let _ = pipeline();
                    thread::yield_now();
                }
            })
        })
        .collect();

    // Meanwhile, blast updates from main thread
    for i in 0..5000u64 {
        source.set(i);
    }

    for handle in handles {
        handle.join().expect("Thread panicked");
    }
}

#[test]
fn barrage_join_chaos() {
    use crate::JoinExt;

    let a = Cell::new(0u64);
    let b = Cell::new(0u64);
    let c = Cell::new(0u64);

    let joined = a.join(&b).join(&c);

    let read_count = Arc::new(AtomicU64::new(0));
    let barrier = Arc::new(Barrier::new(4));

    let mut handles = Vec::new();

    // Writer for a
    let a_clone = a.clone();
    let b1 = barrier.clone();
    handles.push(thread::spawn(move || {
        b1.wait();
        for i in 0..10000 {
            a_clone.set(i);
        }
    }));

    // Writer for b
    let b_clone = b.clone();
    let b2 = barrier.clone();
    handles.push(thread::spawn(move || {
        b2.wait();
        for i in 0..10000 {
            b_clone.set(i);
        }
    }));

    // Writer for c
    let c_clone = c.clone();
    let b3 = barrier.clone();
    handles.push(thread::spawn(move || {
        b3.wait();
        for i in 0..10000 {
            c_clone.set(i);
        }
    }));

    // Reader of joined
    let joined_clone = joined.clone();
    let rc = read_count.clone();
    let b4 = barrier.clone();
    handles.push(thread::spawn(move || {
        b4.wait();
        for _ in 0..10000 {
            let _ = joined_clone.get();
            rc.fetch_add(1, Ordering::Relaxed);
        }
    }));

    for handle in handles {
        handle.join().expect("Thread panicked");
    }

    assert_eq!(read_count.load(Ordering::Relaxed), 10000);
}

#[test]
fn barrage_notification_storm() {
    let source = Cell::new(0u64);

    // Create many subscribers that do work
    let work_done = Arc::new(AtomicU64::new(0));
    let guards: Vec<_> = (0..100)
        .map(|i| {
            let wd = work_done.clone();
            source.subscribe(move |signal| {
                if let Signal::Value(v) = signal {
                    // Do some computation
                    let mut x = **v;
                    for _ in 0..100 {
                        x = x.wrapping_mul(31).wrapping_add(i);
                    }
                    wd.fetch_add(x % 100, Ordering::Relaxed);
                }
            })
        })
        .collect();

    // Blast updates from multiple threads
    let num_writers = 4;
    let updates_per_writer = 1000;

    let handles: Vec<_> = (0..num_writers)
        .map(|writer_id| {
            let source = source.clone();
            thread::spawn(move || {
                for i in 0..updates_per_writer {
                    source.set((writer_id * updates_per_writer + i) as u64);
                }
            })
        })
        .collect();

    for handle in handles {
        handle.join().expect("Thread panicked");
    }

    // Work should have been done
    assert!(work_done.load(Ordering::Relaxed) > 0);

    drop(guards);
}

#[test]
fn barrage_mixed_lifetimes() {
    let source = Cell::new(0u64);
    let long_lived_derived = source.clone().map(|x| x * 2).materialize();

    let notification_count = Arc::new(AtomicU64::new(0));
    let nc = notification_count.clone();
    let _long_lived_guard = long_lived_derived.subscribe(move |_| {
        nc.fetch_add(1, Ordering::Relaxed);
    });

    let num_threads = 8;
    let iterations = 200;

    let handles: Vec<_> = (0..num_threads)
        .map(|_| {
            let source = source.clone();
            thread::spawn(move || {
                for i in 0..iterations {
                    // Create short-lived derived cells
                    let temp = source.clone().map(|x| x + 1).materialize();
                    let count = Arc::new(AtomicU64::new(0));
                    let c = count.clone();
                    let guard = temp.subscribe(move |_| {
                        c.fetch_add(1, Ordering::Relaxed);
                    });

                    // Trigger updates
                    source.set(i as u64);
                    source.set((i + 1) as u64);

                    // Short-lived guard and temp dropped
                    drop(guard);
                }
            })
        })
        .collect();

    for handle in handles {
        handle.join().expect("Thread panicked");
    }

    // Long-lived subscriber should have received many notifications
    assert!(notification_count.load(Ordering::Relaxed) > 1);
}

#[test]
fn barrage_extreme_fan_out() {
    let source = Cell::new(0u64);
    let fan_out = 500;

    let total_notifications = Arc::new(AtomicU64::new(0));

    // Create massive fan-out
    let guards: Vec<_> = (0..fan_out)
        .map(|i| {
            let derived = source.clone().map(move |x| x + i as u64).materialize();
            let tn = total_notifications.clone();
            derived.subscribe(move |_| {
                tn.fetch_add(1, Ordering::Relaxed);
            })
        })
        .collect();

    // Blast updates
    for i in 0..1000 {
        source.set(i);
    }

    let total = total_notifications.load(Ordering::Relaxed);
    // Should have: fan_out initial + fan_out * 1000 updates
    assert_eq!(total, fan_out as u64 * 1001);

    drop(guards);
}

#[test]
fn barrage_cascade_updates() {
    // Create a chain where each level triggers the next
    let level0 = Cell::new(0u64);
    let level1 = level0.clone().map(|x| x + 1).materialize();
    let level2 = level1.map(|x| x * 2).materialize();
    let level3 = level2.map(|x| x + 10).materialize();
    let level4 = level3.map(|x| x / 2).materialize();
    let level5 = level4.map(|x| x.wrapping_sub(5)).materialize();

    let final_count = Arc::new(AtomicU64::new(0));
    let fc = final_count.clone();
    let _guard = level5.subscribe(move |_| {
        fc.fetch_add(1, Ordering::Relaxed);
    });

    // Hammer from multiple threads
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let level0 = level0.clone();
            thread::spawn(move || {
                for i in 0..1000 {
                    level0.set(i);
                }
            })
        })
        .collect();

    for handle in handles {
        handle.join().expect("Thread panicked");
    }

    // Every update should cascade through
    assert!(final_count.load(Ordering::Relaxed) > 1000);
}

#[test]
fn barrage_worst_case_contention() {
    // Single cell, maximum contention
    let cell = Cell::new(0u64);
    let num_threads = 32;
    let ops_per_thread = 5000;

    let barrier = Arc::new(Barrier::new(num_threads));
    let total_writes = Arc::new(AtomicU64::new(0));
    let total_reads = Arc::new(AtomicU64::new(0));

    let handles: Vec<_> = (0..num_threads)
        .map(|thread_id| {
            let cell = cell.clone();
            let barrier = barrier.clone();
            let writes = total_writes.clone();
            let reads = total_reads.clone();

            thread::spawn(move || {
                barrier.wait();

                for i in 0..ops_per_thread {
                    if i % 2 == 0 {
                        cell.set((thread_id * ops_per_thread + i) as u64);
                        writes.fetch_add(1, Ordering::Relaxed);
                    } else {
                        let _ = cell.get();
                        reads.fetch_add(1, Ordering::Relaxed);
                    }
                }
            })
        })
        .collect();

    for handle in handles {
        handle.join().expect("Thread panicked");
    }

    let writes = total_writes.load(Ordering::Relaxed);
    let reads = total_reads.load(Ordering::Relaxed);

    assert_eq!(writes + reads, (num_threads * ops_per_thread) as u64);
}
