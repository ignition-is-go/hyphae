//! Bounded input channel for backpressure at system boundaries.
//!
//! Use `BoundedInput` at ingestion points where external producers may outpace consumers.
//! Supports configurable overflow policies: Block, DropOldest, DropNewest, Error.

use crossbeam::queue::ArrayQueue;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use uuid::Uuid;

use crate::cell::{Cell, CellImmutable, CellMutable};
use crate::signal::Signal;
use crate::subscription::SubscriptionGuard;
use crate::traits::{DepNode, Gettable, Mutable, Watchable};

/// Policy for handling buffer overflow in BoundedInput.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverflowPolicy {
    /// Block the producer until space is available.
    /// Uses spin-wait with yield; best for low-contention scenarios.
    Block,

    /// Drop the oldest value in the buffer to make room.
    DropOldest,

    /// Drop the newest (incoming) value.
    DropNewest,

    /// Complete the cell with an error on overflow.
    Error,
}

/// Metrics for bounded input channels.
#[derive(Debug, Default)]
pub struct BoundedInputMetrics {
    /// Total values successfully pushed
    total_pushed: AtomicU64,

    /// Values dropped due to overflow (DropOldest/DropNewest policies)
    dropped_count: AtomicU64,

    /// Number of times buffer was full (backpressure events)
    backpressure_events: AtomicU64,
}

impl BoundedInputMetrics {
    /// Record a successful push.
    #[inline]
    pub fn record_push(&self) {
        self.total_pushed.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a dropped value.
    #[inline]
    pub fn record_drop(&self) {
        self.dropped_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a backpressure event (buffer full).
    #[inline]
    pub fn record_backpressure(&self) {
        self.backpressure_events.fetch_add(1, Ordering::Relaxed);
    }

    /// Get total values successfully pushed.
    pub fn total_pushed(&self) -> u64 {
        self.total_pushed.load(Ordering::Relaxed)
    }

    /// Get count of dropped values.
    pub fn dropped_count(&self) -> u64 {
        self.dropped_count.load(Ordering::Relaxed)
    }

    /// Get count of backpressure events.
    pub fn backpressure_events(&self) -> u64 {
        self.backpressure_events.load(Ordering::Relaxed)
    }
}

/// Inner state shared between BoundedInput clones.
struct BoundedInputInner<T> {
    /// Lock-free bounded buffer
    buffer: ArrayQueue<T>,

    /// The downstream cell that receives values
    cell: Cell<T, CellMutable>,

    /// Overflow handling policy
    policy: OverflowPolicy,

    /// Whether the input has been closed
    closed: AtomicBool,

    /// Metrics for monitoring backpressure
    metrics: BoundedInputMetrics,

    /// Capacity for reference
    capacity: usize,
}

/// A bounded input channel that buffers values before forwarding to a Cell.
///
/// Use this at system boundaries where external producers may outpace consumers.
/// The buffer uses a lock-free `ArrayQueue` from crossbeam.
///
/// # Example
///
/// ```
/// use hypha::{BoundedInput, OverflowPolicy, Watchable, Signal};
///
/// let input = BoundedInput::new(0, 10, OverflowPolicy::DropOldest);
///
/// // Subscribe to receive values
/// let _guard = input.subscribe(|signal| {
///     if let Signal::Value(v) = signal {
///         println!("Received: {}", v);
///     }
/// });
///
/// // Push values (may drop oldest if buffer fills)
/// for i in 1..=20 {
///     let _ = input.push(i);
/// }
///
/// // Check metrics
/// println!("Dropped: {}", input.metrics().dropped_count());
/// ```
pub struct BoundedInput<T> {
    inner: Arc<BoundedInputInner<T>>,
}

impl<T> Clone for BoundedInput<T> {
    fn clone(&self) -> Self {
        BoundedInput {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<T: Clone + Send + Sync + 'static> BoundedInput<T> {
    /// Create a new bounded input with the given capacity and overflow policy.
    ///
    /// # Panics
    ///
    /// Panics if `capacity` is zero.
    pub fn new(initial_value: T, capacity: usize, policy: OverflowPolicy) -> Self {
        assert!(capacity > 0, "capacity must be positive");

        BoundedInput {
            inner: Arc::new(BoundedInputInner {
                buffer: ArrayQueue::new(capacity),
                cell: Cell::new(initial_value),
                policy,
                closed: AtomicBool::new(false),
                metrics: BoundedInputMetrics::default(),
                capacity,
            }),
        }
    }

    /// Push a value into the bounded input.
    ///
    /// Values are buffered and forwarded to subscribers when `flush()` is called
    /// or when the buffer is accessed via `get()`.
    ///
    /// Behavior depends on the overflow policy when the buffer is full:
    /// - `Block`: Spin-waits until space is available (requires external flush)
    /// - `DropOldest`: Drops the oldest buffered value
    /// - `DropNewest`: Drops the incoming value
    /// - `Error`: Completes the cell with an error
    ///
    /// Returns `Ok(())` on success, `Err(value)` if the value was rejected.
    pub fn push(&self, value: T) -> Result<(), T> {
        if self.inner.closed.load(Ordering::SeqCst) {
            return Err(value);
        }

        match self.inner.buffer.push(value) {
            Ok(()) => {
                self.inner.metrics.record_push();
                Ok(())
            }
            Err(value) => {
                // Buffer is full - handle according to policy
                self.inner.metrics.record_backpressure();
                self.handle_overflow(value)
            }
        }
    }

    /// Push a value and immediately flush to subscribers.
    ///
    /// This is a convenience method equivalent to `push()` followed by `flush()`.
    pub fn push_flush(&self, value: T) -> Result<(), T> {
        let result = self.push(value);
        self.flush();
        result
    }

    fn handle_overflow(&self, value: T) -> Result<(), T> {
        match self.inner.policy {
            OverflowPolicy::Block => {
                // Spin-wait until space available
                let mut current_value = value;
                loop {
                    if self.inner.closed.load(Ordering::SeqCst) {
                        return Err(current_value);
                    }
                    match self.inner.buffer.push(current_value) {
                        Ok(()) => {
                            self.inner.metrics.record_push();
                            return Ok(());
                        }
                        Err(v) => {
                            current_value = v;
                            // Yield to allow consumer progress (they should flush)
                            std::thread::yield_now();
                        }
                    }
                }
            }
            OverflowPolicy::DropOldest => {
                // Pop oldest, push new
                let _ = self.inner.buffer.pop();
                self.inner.metrics.record_drop();
                match self.inner.buffer.push(value) {
                    Ok(()) => {
                        self.inner.metrics.record_push();
                        Ok(())
                    }
                    Err(v) => {
                        // Shouldn't happen after pop, but handle gracefully
                        Err(v)
                    }
                }
            }
            OverflowPolicy::DropNewest => {
                // Simply drop the incoming value
                self.inner.metrics.record_drop();
                Ok(())
            }
            OverflowPolicy::Error => {
                // Complete the cell with an error
                self.inner.cell.fail(anyhow::anyhow!(
                    "BoundedInput overflow: buffer full (capacity {})",
                    self.inner.capacity
                ));
                Err(value)
            }
        }
    }

    /// Flush all buffered values to subscribers.
    ///
    /// This drains the buffer and notifies all subscribers of each value.
    pub fn flush(&self) {
        while let Some(value) = self.inner.buffer.pop() {
            self.inner.cell.set(value);
        }
    }

    /// Close the input, preventing new values.
    ///
    /// Flushes any remaining buffered values and completes the downstream cell.
    pub fn close(&self) {
        self.inner.closed.store(true, Ordering::SeqCst);
        self.flush();
        self.inner.cell.complete();
    }

    /// Get metrics for this bounded input.
    pub fn metrics(&self) -> &BoundedInputMetrics {
        &self.inner.metrics
    }

    /// Get the buffer capacity.
    pub fn capacity(&self) -> usize {
        self.inner.capacity
    }

    /// Check if the input has been closed.
    pub fn is_closed(&self) -> bool {
        self.inner.closed.load(Ordering::SeqCst)
    }

    /// Convert to an immutable watchable cell.
    ///
    /// The returned cell can be used with operators but the original
    /// `BoundedInput` can still receive values via `push()`.
    pub fn to_cell(&self) -> Cell<T, CellImmutable> {
        self.inner.cell.clone().lock()
    }
}

// ============================================================================
// Trait implementations
// ============================================================================

impl<T: Clone + Send + Sync + 'static> Gettable<T> for BoundedInput<T> {
    fn get(&self) -> T {
        // Flush buffer first so the latest value is visible
        self.flush();
        self.inner.cell.get()
    }
}

impl<T: Clone + Send + Sync + 'static> DepNode for BoundedInput<T> {
    fn id(&self) -> Uuid {
        self.inner.cell.inner.id
    }

    fn name(&self) -> Option<String> {
        (**self.inner.cell.inner.name.load())
            .as_ref()
            .map(|s| s.to_string())
    }

    fn deps(&self) -> Vec<Arc<dyn DepNode>> {
        vec![] // BoundedInput is a source, no dependencies
    }
}

impl<T: Clone + Send + Sync + 'static> Watchable<T> for BoundedInput<T> {
    fn subscribe(&self, callback: impl Fn(&Signal<T>) + Send + Sync + 'static) -> SubscriptionGuard {
        self.inner.cell.subscribe(callback)
    }

    fn unsubscribe(&self, id: Uuid) {
        self.inner.cell.unsubscribe(id)
    }

    fn is_complete(&self) -> bool {
        self.inner.cell.is_complete()
    }

    fn is_error(&self) -> bool {
        self.inner.cell.is_error()
    }

    fn error(&self) -> Option<Arc<anyhow::Error>> {
        self.inner.cell.error()
    }
}
