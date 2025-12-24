//! Lock-free metrics for Cell notifications.
//!
//! Provides observability into cell performance including notification counts,
//! timing, and subscriber tracking.

use std::sync::atomic::{AtomicU64, Ordering};

/// Lock-free metrics for Cell notifications.
///
/// All fields use atomic operations for thread-safe updates without locks.
/// Use `Ordering::Relaxed` for counters as exact ordering isn't critical for monitoring.
#[derive(Debug, Default)]
pub struct CellMetrics {
    /// Total number of notify() calls
    notify_count: AtomicU64,

    /// Total time spent in notify() calls (nanoseconds)
    total_notify_time_ns: AtomicU64,

    /// Slowest single subscriber callback (nanoseconds)
    slowest_subscriber_ns: AtomicU64,

    /// Current number of subscribers
    subscriber_count: AtomicU64,

    /// Duration of the last notify() call (nanoseconds)
    last_notify_time_ns: AtomicU64,
}

impl CellMetrics {
    /// Create a new metrics instance with all counters at zero.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a notify call with its duration.
    #[inline]
    pub fn record_notify(&self, duration_ns: u64) {
        self.notify_count.fetch_add(1, Ordering::Relaxed);
        self.total_notify_time_ns
            .fetch_add(duration_ns, Ordering::Relaxed);
        self.last_notify_time_ns
            .store(duration_ns, Ordering::Relaxed);
    }

    /// Update slowest subscriber using compare-and-swap to track the maximum.
    #[inline]
    pub fn update_slowest_subscriber(&self, duration_ns: u64) {
        let mut current = self.slowest_subscriber_ns.load(Ordering::Relaxed);
        while duration_ns > current {
            match self.slowest_subscriber_ns.compare_exchange_weak(
                current,
                duration_ns,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => current = actual,
            }
        }
    }

    /// Record that a subscriber was added.
    #[inline]
    pub fn record_subscriber_added(&self) {
        self.subscriber_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Record that a subscriber was removed.
    #[inline]
    pub fn record_subscriber_removed(&self) {
        self.subscriber_count.fetch_sub(1, Ordering::Relaxed);
    }

    /// Get the total number of notify() calls.
    pub fn notify_count(&self) -> u64 {
        self.notify_count.load(Ordering::Relaxed)
    }

    /// Get the total time spent in notify() calls (nanoseconds).
    pub fn total_notify_time_ns(&self) -> u64 {
        self.total_notify_time_ns.load(Ordering::Relaxed)
    }

    /// Get the average time per notify() call (nanoseconds).
    pub fn avg_notify_time_ns(&self) -> u64 {
        let count = self.notify_count();
        if count == 0 {
            0
        } else {
            self.total_notify_time_ns() / count
        }
    }

    /// Get the slowest subscriber callback time (nanoseconds).
    pub fn slowest_subscriber_ns(&self) -> u64 {
        self.slowest_subscriber_ns.load(Ordering::Relaxed)
    }

    /// Get the current number of subscribers.
    pub fn subscriber_count(&self) -> u64 {
        self.subscriber_count.load(Ordering::Relaxed)
    }

    /// Get the duration of the last notify() call (nanoseconds).
    pub fn last_notify_time_ns(&self) -> u64 {
        self.last_notify_time_ns.load(Ordering::Relaxed)
    }

    /// Reset timing metrics (notify_count, total_notify_time_ns, slowest_subscriber_ns, last_notify_time_ns).
    ///
    /// Note: subscriber_count is NOT reset as it tracks current state.
    pub fn reset_timing(&self) {
        self.notify_count.store(0, Ordering::Relaxed);
        self.total_notify_time_ns.store(0, Ordering::Relaxed);
        self.slowest_subscriber_ns.store(0, Ordering::Relaxed);
        self.last_notify_time_ns.store(0, Ordering::Relaxed);
    }
}
