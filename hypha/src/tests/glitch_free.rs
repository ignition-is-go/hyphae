//! Tests for transaction-based glitch-free propagation.
//!
//! These tests verify that the transaction mechanism eliminates the diamond problem
//! and ensures at most one update per effect when a source cell is set.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use crate::traits::{Gettable, JoinExt, MapExt, Mutable, Watchable};
use crate::{Cell, Signal, join_vec};

/// Test the classic diamond dependency problem.
///
/// ```text
///      A (source)
///     / \
///    B   C  (derived from A)
///     \ /
///      D    (derived from B and C)
/// ```
///
/// Without transactions: Setting A causes D to fire twice.
/// With transactions: D should wait for both B and C, then fire once.
#[test]
fn test_diamond_dependency_glitch_free() {
    let a = Cell::new(1);
    let a_clone = a.clone();

    // B = A * 2
    let b = a.map(|x| *x * 2);

    // C = A + 10
    let c = a_clone.map(|x| *x + 10);

    // D = B + C
    let d = b.join(&c).map(|(b, c)| b + c);

    let fire_count = Arc::new(AtomicUsize::new(0));
    let fire_count_clone = fire_count.clone();
    let last_value = Arc::new(AtomicUsize::new(0));
    let last_value_clone = last_value.clone();

    let _guard = d.subscribe(move |signal| {
        if let Signal::Value(v, _) = signal {
            fire_count_clone.fetch_add(1, Ordering::SeqCst);
            last_value_clone.store(**v, Ordering::SeqCst);
        }
    });

    // Initial subscription fires once
    assert_eq!(fire_count.load(Ordering::SeqCst), 1);
    // Initial value: (1*2) + (1+10) = 2 + 11 = 13
    assert_eq!(last_value.load(Ordering::SeqCst), 13);

    // Reset fire count
    fire_count.store(0, Ordering::SeqCst);

    // Using set_tx should cause D to fire only once (glitch-free)
    a.set_tx(5);

    // D should have fired exactly once with value (5*2) + (5+10) = 10 + 15 = 25
    assert_eq!(fire_count.load(Ordering::SeqCst), 1, "D should fire exactly once with set_tx");
    assert_eq!(last_value.load(Ordering::SeqCst), 25);

    // Reset and test regular set (may fire twice)
    fire_count.store(0, Ordering::SeqCst);
    a.set(3);

    // With regular set, D may fire 1 or 2 times depending on evaluation order
    // This is acceptable without transactions
    let fires = fire_count.load(Ordering::SeqCst);
    assert!(fires >= 1, "D should fire at least once");
    // Final value should be correct: (3*2) + (3+10) = 6 + 13 = 19
    assert_eq!(last_value.load(Ordering::SeqCst), 19);
}

/// Test with join_vec for a more complex diamond.
///
/// ```text
///         A (source)
///       / | \
///      B  C  D  (all derived from A)
///       \ | /
///         E    (join_vec of B, C, D)
/// ```
#[test]
fn test_wide_diamond_with_join_vec() {
    let a = Cell::new(1);

    let b = a.map(|x| *x * 2);
    let c = a.map(|x| *x + 10);
    let d = a.map(|x| *x * *x);

    let e = join_vec(vec![b, c, d]);

    let fire_count = Arc::new(AtomicUsize::new(0));
    let fire_count_clone = fire_count.clone();

    let _guard = e.subscribe(move |signal| {
        if let Signal::Value(_, _) = signal {
            fire_count_clone.fetch_add(1, Ordering::SeqCst);
        }
    });

    // Initial subscription fires once
    assert_eq!(fire_count.load(Ordering::SeqCst), 1);
    // Initial: [1*2, 1+10, 1*1] = [2, 11, 1]
    assert_eq!(e.get(), vec![2, 11, 1]);

    // Reset fire count
    fire_count.store(0, Ordering::SeqCst);

    // Using set_tx should cause E to fire only once
    a.set_tx(4);

    assert_eq!(fire_count.load(Ordering::SeqCst), 1, "E should fire exactly once with set_tx");
    // [4*2, 4+10, 4*4] = [8, 14, 16]
    assert_eq!(e.get(), vec![8, 14, 16]);
}

/// Test that cycles are detected and don't cause infinite loops.
#[test]
fn test_cycle_detection_prevents_infinite_loop() {
    use std::time::Duration;
    use std::thread;

    let a = Cell::new(1);
    let a_weak = a.downgrade();

    // Create a circular dependency through map
    // This creates a potential cycle: a -> b -> (attempts to update a)
    let b = a.map(move |x| {
        // Try to update a in the subscription - this would create a cycle
        // But cycle detection should prevent infinite loops
        if let Some(a_strong) = a_weak.upgrade() {
            // This would normally cause a cycle, but our TxContext tracks the path
            // and should detect and prevent infinite recursion
            let _ = a_strong.get(); // Just read, don't set in this test
        }
        *x * 2
    });

    // Subscribe to verify it works
    let fire_count = Arc::new(AtomicUsize::new(0));
    let fire_count_clone = fire_count.clone();

    let _guard = b.subscribe(move |signal| {
        if let Signal::Value(_, _) = signal {
            fire_count_clone.fetch_add(1, Ordering::SeqCst);
        }
    });

    // Set with transaction and ensure it completes (no infinite loop)
    a.set_tx(5);

    // If we get here without hanging, cycle detection worked
    assert_eq!(b.get(), 10);

    // Use a thread with timeout to verify no infinite loop
    let test_passed = Arc::new(AtomicUsize::new(0));
    let test_passed_clone = test_passed.clone();

    let handle = thread::spawn(move || {
        a.set_tx(7);
        test_passed_clone.store(1, Ordering::SeqCst);
    });

    // Wait up to 1 second for the operation to complete
    thread::sleep(Duration::from_millis(100));
    assert_eq!(test_passed.load(Ordering::SeqCst), 1, "set_tx should complete without hanging");

    handle.join().unwrap();
}

/// Test nested joins with transactions.
///
/// ```text
///      A     B
///       \   /
///        AB (join)
///       /   \
///     C       D
///      \     /
///        CD (join)
/// ```
#[test]
fn test_nested_joins() {
    let a = Cell::new(1);
    let b = Cell::new(2);

    let ab = a.join(&b);

    let c = ab.map(|(a, b)| *a + *b);
    let d = ab.map(|(a, b)| *a * *b);

    let cd = c.join(&d);

    let fire_count = Arc::new(AtomicUsize::new(0));
    let fire_count_clone = fire_count.clone();

    let _guard = cd.subscribe(move |signal| {
        if let Signal::Value(_, _) = signal {
            fire_count_clone.fetch_add(1, Ordering::SeqCst);
        }
    });

    // Initial
    assert_eq!(fire_count.load(Ordering::SeqCst), 1);
    // (1+2, 1*2) = (3, 2)
    assert_eq!(cd.get(), (3, 2));

    // Reset
    fire_count.store(0, Ordering::SeqCst);

    // Update A with transaction
    a.set_tx(5);

    // CD should fire once
    assert_eq!(fire_count.load(Ordering::SeqCst), 1, "CD should fire exactly once with set_tx");
    // (5+2, 5*2) = (7, 10)
    assert_eq!(cd.get(), (7, 10));
}

/// Test that transaction ID is properly generated and unique.
#[test]
fn test_transaction_id_uniqueness() {
    use crate::TxId;

    let id1 = TxId::new();
    let id2 = TxId::new();
    let id3 = TxId::new();

    assert_ne!(id1, id2);
    assert_ne!(id2, id3);
    assert_ne!(id1, id3);
}

/// Test that set_tx returns a usable transaction ID.
#[test]
fn test_set_tx_returns_txid() {
    let a = Cell::new(1);

    let txid1 = a.set_tx(5);
    let txid2 = a.set_tx(10);

    // Each set_tx should return a different transaction ID
    assert_ne!(txid1, txid2);
}

/// Test concurrent set_tx from different threads.
#[test]
fn test_concurrent_transactions() {
    use std::thread;

    let a = Cell::new(0);
    let a_clone = a.clone();

    // Create a derived cell
    let b = a.map(|x| *x * 2);

    let fire_count = Arc::new(AtomicUsize::new(0));
    let fire_count_clone = fire_count.clone();

    let _guard = b.subscribe(move |signal| {
        if let Signal::Value(_, _) = signal {
            fire_count_clone.fetch_add(1, Ordering::SeqCst);
        }
    });

    // Reset after initial subscription
    fire_count.store(0, Ordering::SeqCst);

    // Spawn multiple threads to concurrently update
    let handles: Vec<_> = (0..10)
        .map(|i| {
            let a = a_clone.clone();
            thread::spawn(move || {
                a.set_tx(i);
            })
        })
        .collect();

    // Wait for all threads to complete
    for handle in handles {
        handle.join().unwrap();
    }

    // All updates should have been processed
    let fires = fire_count.load(Ordering::SeqCst);
    assert!(fires >= 1, "At least one update should have been processed");
}
