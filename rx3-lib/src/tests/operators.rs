use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use std::thread;

use crate::{
    Cell, Mutable, Gettable, MapExt, FilterExt, ScanExt, TapExt,
    TakeExt, SkipExt, PairwiseExt, DedupedExt, JoinExt, DepNode,
    DebounceExt, ThrottleExt, DelayExt, SwitchMapExt, MergeMapExt,
};
use crate::traits::Watchable;

// ============================================================================
// Map
// ============================================================================

#[test]
fn test_map_transform() {
    let source = Cell::new(5);
    let doubled = source.map(|x| x * 2);

    assert_eq!(doubled.get(), 10);

    source.set(10);
    assert_eq!(doubled.get(), 20);
}

#[test]
fn test_map_chain() {
    let source = Cell::new(1);
    let result = source
        .map(|x| x + 1)
        .map(|x| x * 2)
        .map(|x| x + 10);

    assert_eq!(result.get(), 14); // ((1 + 1) * 2) + 10

    source.set(5);
    assert_eq!(result.get(), 22); // ((5 + 1) * 2) + 10
}

#[test]
fn test_map_type_change() {
    let source = Cell::new(42);
    let stringified = source.map(|x| format!("value: {}", x));

    assert_eq!(stringified.get(), "value: 42");
}

#[test]
fn test_map_tracks_dependency() {
    let source = Cell::new(1).with_name("source");
    let mapped = source.map(|x| x * 2);

    assert_eq!(mapped.dependency_count(), 1);
    assert!(mapped.has_dependencies());
}

// ============================================================================
// Filter
// ============================================================================

#[test]
fn test_filter_passes_matching() {
    let source = Cell::new(10);
    let evens = source.filter(|x| x % 2 == 0);
    let received = Arc::new(AtomicU64::new(0));

    let r = received.clone();
    evens.watch(move |v| {
        r.store(*v, Ordering::SeqCst);
    });

    assert_eq!(received.load(Ordering::SeqCst), 10);

    source.set(4);
    assert_eq!(received.load(Ordering::SeqCst), 4);
}

#[test]
fn test_filter_blocks_non_matching() {
    let source = Cell::new(10u64);
    let evens = source.filter(|x| x % 2 == 0);
    let received = Arc::new(AtomicU64::new(0));

    let r = received.clone();
    evens.watch(move |v| {
        r.store(*v, Ordering::SeqCst);
    });

    source.set(3); // odd - should not pass
    assert_eq!(received.load(Ordering::SeqCst), 10); // still 10

    source.set(6); // even - should pass
    assert_eq!(received.load(Ordering::SeqCst), 6);
}

// ============================================================================
// Scan
// ============================================================================

#[test]
fn test_scan_accumulates() {
    let source = Cell::new(1u64);
    let sum = source.scan(0u64, |acc, x| acc + x);

    // Initial: 0 + 1 = 1
    assert_eq!(sum.get(), 1);

    source.set(2);
    assert_eq!(sum.get(), 3); // 1 + 2

    source.set(3);
    assert_eq!(sum.get(), 6); // 3 + 3
}

#[test]
fn test_scan_with_different_types() {
    let source = Cell::new(1);
    let collected = source.scan(String::new(), |acc, x| format!("{}{}", acc, x));

    assert_eq!(collected.get(), "1");

    source.set(2);
    assert_eq!(collected.get(), "12");

    source.set(3);
    assert_eq!(collected.get(), "123");
}

// ============================================================================
// Tap
// ============================================================================

#[test]
fn test_tap_side_effect() {
    let source = Cell::new(0u64);
    let side_effect = Arc::new(AtomicU64::new(0));

    let se = side_effect.clone();
    let tapped = source.tap(move |v| {
        se.store(*v, Ordering::SeqCst);
    });

    source.set(42);
    assert_eq!(side_effect.load(Ordering::SeqCst), 42);
    assert_eq!(tapped.get(), 42); // value passes through unchanged
}

// ============================================================================
// Take
// ============================================================================

#[test]
fn test_take_limits_emissions() {
    let source = Cell::new(0u64);
    let taken = source.take(3);
    let count = Arc::new(AtomicU64::new(0));

    let c = count.clone();
    taken.watch(move |_| {
        c.fetch_add(1, Ordering::SeqCst);
    });

    // Initial watch call counts as 1
    assert_eq!(count.load(Ordering::SeqCst), 1);

    source.set(1); // 2
    source.set(2); // 3
    source.set(3); // ignored
    source.set(4); // ignored

    assert_eq!(count.load(Ordering::SeqCst), 3);
}

// ============================================================================
// Skip
// ============================================================================

#[test]
fn test_skip_ignores_first_n() {
    let source = Cell::new(0u64);
    let skipped = source.skip(2);
    let count = Arc::new(AtomicU64::new(0));

    let c = count.clone();
    skipped.watch(move |_| {
        c.fetch_add(1, Ordering::SeqCst);
    });

    // Watch always fires once immediately with current value
    assert_eq!(count.load(Ordering::SeqCst), 1);

    // First set - skipped (to_skip goes 2 -> 1 from initial watch, then 1 -> 0 here)
    source.set(1);
    assert_eq!(count.load(Ordering::SeqCst), 1);

    // Second set - now emits (to_skip is 0)
    source.set(2);
    assert_eq!(count.load(Ordering::SeqCst), 2);

    // Third set - emits
    source.set(3);
    assert_eq!(count.load(Ordering::SeqCst), 3);
}

// ============================================================================
// Pairwise
// ============================================================================

#[test]
fn test_pairwise_emits_pairs() {
    let source = Cell::new(1u64);
    let pairs = source.pairwise();

    source.set(2);
    assert_eq!(pairs.get(), (1, 2));

    source.set(3);
    assert_eq!(pairs.get(), (2, 3));
}

// ============================================================================
// Deduped
// ============================================================================

#[test]
fn test_deduped_blocks_duplicates() {
    let source = Cell::new(1u64);
    let deduped = source.deduped();
    let count = Arc::new(AtomicU64::new(0));

    let c = count.clone();
    deduped.watch(move |_| {
        c.fetch_add(1, Ordering::SeqCst);
    });

    assert_eq!(count.load(Ordering::SeqCst), 1); // initial

    source.set(1); // same value - blocked
    assert_eq!(count.load(Ordering::SeqCst), 1);

    source.set(2); // different - passes
    assert_eq!(count.load(Ordering::SeqCst), 2);

    source.set(2); // same - blocked
    assert_eq!(count.load(Ordering::SeqCst), 2);
}

// ============================================================================
// Join
// ============================================================================

#[test]
fn test_join_combines_cells() {
    let a = Cell::new(1);
    let b = Cell::new("hello");

    let joined = a.join(&b);
    assert_eq!(joined.get(), (1, "hello"));

    a.set(2);
    assert_eq!(joined.get(), (2, "hello"));

    b.set("world");
    assert_eq!(joined.get(), (2, "world"));
}

#[test]
fn test_flat_macro_tree_joins() {
    use crate::flat;

    let a = Cell::new(1);
    let b = Cell::new(2);
    let c = Cell::new(3);
    let d = Cell::new(4);

    // Tree join: ab.join(&cd) produces ((A, B), (C, D))
    let ab = a.join(&b);
    let cd = c.join(&d);
    let sum = ab.join(&cd).map(flat!(|(a, b), (c, d)| a + b + c + d));

    assert_eq!(sum.get(), 10);
}

#[test]
fn test_flat_macro_chain() {
    use crate::flat;

    let a = Cell::new(1);
    let b = Cell::new(2);
    let c = Cell::new(3);

    // Chain: a.join(&b).join(&c) produces ((A, B), C)
    let sum = a.join(&b).join(&c).map(flat!(|(x, y), z| x + y + z));

    assert_eq!(sum.get(), 6);
}

// ============================================================================
// Debounce
// ============================================================================

#[test]
fn test_debounce_waits_for_pause() {
    let source = Cell::new(0u64);
    let debounced = source.debounce(Duration::from_millis(50));
    let received = Arc::new(AtomicU64::new(0));

    let r = received.clone();
    debounced.watch(move |v| {
        r.store(*v, Ordering::SeqCst);
    });

    // Rapid updates
    source.set(1);
    source.set(2);
    source.set(3);

    // Should not have updated yet
    thread::sleep(Duration::from_millis(10));
    assert_eq!(received.load(Ordering::SeqCst), 0);

    // Wait for debounce
    thread::sleep(Duration::from_millis(100));
    assert_eq!(received.load(Ordering::SeqCst), 3);
}

// ============================================================================
// Throttle
// ============================================================================

#[test]
fn test_throttle_limits_rate() {
    let source = Cell::new(0u64);
    let throttled = source.throttle(Duration::from_millis(50));
    let count = Arc::new(AtomicU64::new(0));

    let c = count.clone();
    throttled.watch(move |_| {
        c.fetch_add(1, Ordering::SeqCst);
    });

    // Rapid updates
    for i in 1..=10 {
        source.set(i);
    }

    // Should have limited emissions
    let emissions = count.load(Ordering::SeqCst);
    assert!(emissions < 10, "throttle should limit emissions, got {}", emissions);
}

// ============================================================================
// Delay
// ============================================================================

#[test]
fn test_delay_delays_emission() {
    let source = Cell::new(0u64);
    let delayed = source.delay(Duration::from_millis(50));
    let received = Arc::new(AtomicU64::new(0));

    let r = received.clone();
    delayed.watch(move |v| {
        r.store(*v, Ordering::SeqCst);
    });

    // Wait for the initial delayed value (0) to arrive before triggering a new one.
    // This avoids a race between the initial and new value's delayed notifications.
    thread::sleep(Duration::from_millis(100));
    assert_eq!(received.load(Ordering::SeqCst), 0);

    source.set(42);

    // Not yet (delay is 50ms, so after 20ms value should still be 0)
    thread::sleep(Duration::from_millis(20));
    assert_eq!(received.load(Ordering::SeqCst), 0);

    // Now (wait 100ms more to ensure delay has passed with margin for thread scheduling)
    thread::sleep(Duration::from_millis(100));
    assert_eq!(received.load(Ordering::SeqCst), 42);
}

// ============================================================================
// SwitchMap
// ============================================================================

#[test]
fn test_switch_map_switches() {
    let source = Cell::new(1u64);
    let switched = source.switch_map(|v| {
        let v = *v;
        Cell::new(v * 10).map(move |x| x + v)
    });

    // Initial: 1 * 10 + 1 = 11
    assert_eq!(switched.get(), 11);
}

// ============================================================================
// MergeMap
// ============================================================================

#[test]
fn test_merge_map_merges() {
    let source = Cell::new(1u64);
    let merged = source.merge_map(|v| {
        // Must return CellImmutable, use map to create one
        Cell::new(*v).map(|x| x * 10)
    });

    assert_eq!(merged.get(), 10);
}
