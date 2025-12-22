use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use crate::{Cell, Gettable, MapExt, Mutable, Signal};
use crate::traits::Watchable;

// ============================================================================
// WeakCell Tests
// ============================================================================

#[test]
fn test_weak_cell_upgrade_succeeds_while_cell_alive() {
    let cell = Cell::new(42u64);
    let weak = cell.downgrade();

    // While cell is alive, upgrade should succeed
    let upgraded = weak.upgrade();
    assert!(upgraded.is_some());
    assert_eq!(upgraded.unwrap().get(), 42);
}

#[test]
fn test_weak_cell_upgrade_fails_after_drop() {
    let weak = {
        let cell = Cell::new(42u64);
        cell.downgrade()
    }; // cell is dropped here

    // After cell is dropped, upgrade should fail
    assert!(weak.upgrade().is_none());
}

#[test]
fn test_weak_cell_clone() {
    let cell = Cell::new(42u64);
    let weak1 = cell.downgrade();
    let weak2 = weak1.clone();

    // Both should upgrade successfully
    assert!(weak1.upgrade().is_some());
    assert!(weak2.upgrade().is_some());

    drop(cell);

    // Both should fail after drop
    assert!(weak1.upgrade().is_none());
    assert!(weak2.upgrade().is_none());
}

// ============================================================================
// Disposal Behavior Tests
// ============================================================================

#[test]
fn test_derived_cell_drop_stops_notifications() {
    let source = Cell::new(0u64);
    let call_count = Arc::new(AtomicU64::new(0));

    {
        let count = call_count.clone();
        let _derived = source.map(move |v| {
            count.fetch_add(1, Ordering::SeqCst);
            *v * 2
        });

        // Map callback called once on creation
        assert_eq!(call_count.load(Ordering::SeqCst), 1);

        source.set(1);
        // Map callback called again
        assert_eq!(call_count.load(Ordering::SeqCst), 2);
    } // derived is dropped here

    // After dropping derived, setting source should NOT trigger the map callback
    source.set(2);
    assert_eq!(call_count.load(Ordering::SeqCst), 2); // Still 2, not incremented
}

#[test]
fn test_source_still_works_after_derived_dropped() {
    let source = Cell::new(0u64);
    let source_call_count = Arc::new(AtomicU64::new(0));

    // Add a direct watcher on source
    let count = source_call_count.clone();
    let _guard = source.subscribe(move |_| {
        count.fetch_add(1, Ordering::SeqCst);
    });

    {
        let _derived = source.map(|v| *v * 2);
        source.set(1);
    } // derived dropped

    // Source should still work for other watchers
    source.set(2);
    // Initial watch (0) + set(1) + set(2) = 3 calls
    assert_eq!(source_call_count.load(Ordering::SeqCst), 3);
}

#[test]
fn test_chained_operators_drop_correctly() {
    let source = Cell::new(0u64);
    let map1_count = Arc::new(AtomicU64::new(0));
    let map2_count = Arc::new(AtomicU64::new(0));

    let c1 = map1_count.clone();
    let c2 = map2_count.clone();

    {
        let _final = source
            .map(move |v| { c1.fetch_add(1, Ordering::SeqCst); *v * 2 })
            .map(move |v| { c2.fetch_add(1, Ordering::SeqCst); *v + 1 });

        // Both maps called once on creation
        assert_eq!(map1_count.load(Ordering::SeqCst), 1);
        assert_eq!(map2_count.load(Ordering::SeqCst), 1);

        source.set(1);
        assert_eq!(map1_count.load(Ordering::SeqCst), 2);
        assert_eq!(map2_count.load(Ordering::SeqCst), 2);
    } // both derived cells dropped

    source.set(2);
    // Neither map should be called anymore
    assert_eq!(map1_count.load(Ordering::SeqCst), 2);
    assert_eq!(map2_count.load(Ordering::SeqCst), 2);
}

#[test]
fn test_parent_cell_outlives_derived() {
    let received = Arc::new(AtomicU64::new(0));
    let r = received.clone();

    let source = Cell::new(0u64);
    let derived = source.map(|v| *v * 2);

    let _guard = derived.subscribe(move |signal| {
        if let Signal::Value(v) = signal {
            r.store(**v, Ordering::SeqCst);
        }
    });

    source.set(5);
    assert_eq!(received.load(Ordering::SeqCst), 10);

    // Derived still holds strong ref to source via dependencies,
    // so source stays alive even if we drop our reference
    drop(source);

    // derived should still have its value
    assert_eq!(derived.get(), 10);
}

// ============================================================================
// Subscription Cleanup Tests
// ============================================================================

#[test]
fn test_subscription_cleaned_up_on_derived_drop() {
    let source = Cell::new(0u64);

    // Check initial subscriber count
    let initial_count = source.inner.subscribers.len();

    {
        let _derived = source.map(|v| *v * 2);
        // map() creates one subscription on source
        assert_eq!(source.inner.subscribers.len(), initial_count + 1);
    } // derived dropped here - should unsubscribe

    // Subscription should be cleaned up
    assert_eq!(source.inner.subscribers.len(), initial_count);
}

#[test]
fn test_chained_subscriptions_cleaned_up() {
    let source = Cell::new(0u64);
    let initial_count = source.inner.subscribers.len();

    {
        let derived1 = source.map(|v| *v * 2);
        let d1_initial = derived1.inner.subscribers.len();

        {
            let _derived2 = derived1.map(|v| *v + 1);
            // derived2 subscribes to derived1
            assert_eq!(derived1.inner.subscribers.len(), d1_initial + 1);
        } // derived2 dropped

        // derived1 subscription should be cleaned up
        assert_eq!(derived1.inner.subscribers.len(), d1_initial);
    } // derived1 dropped

    // source subscription should be cleaned up
    assert_eq!(source.inner.subscribers.len(), initial_count);
}

#[test]
fn test_multiple_derived_cells_independent_cleanup() {
    let source = Cell::new(0u64);
    let initial_count = source.inner.subscribers.len();

    let derived1 = source.map(|v| *v * 2);
    assert_eq!(source.inner.subscribers.len(), initial_count + 1);

    let derived2 = source.map(|v| *v + 1);
    assert_eq!(source.inner.subscribers.len(), initial_count + 2);

    drop(derived1);
    // Only derived1's subscription should be cleaned up
    assert_eq!(source.inner.subscribers.len(), initial_count + 1);

    drop(derived2);
    // Now both should be cleaned up
    assert_eq!(source.inner.subscribers.len(), initial_count);
}
