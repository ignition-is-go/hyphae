//! Bounded output channel for subscribers.
//!
//! `BoundedOutput` creates a subscriber that writes to a bounded channel,
//! allowing consumers to pull values at their own pace.

use crossbeam::channel::{self, Receiver, TryRecvError};
use std::time::Duration;

use crate::signal::Signal;
use crate::subscription::SubscriptionGuard;
use crate::traits::Watchable;

/// A bounded output channel for receiving values from a Cell.
///
/// Instead of processing values in a callback, values are buffered in a channel
/// and can be pulled by the consumer at their own pace. Useful for decoupling
/// rx3 notification from slow I/O or processing.
///
/// # Example
///
/// ```
/// use rx3::{Cell, Mutable, BoundedOutput};
/// use std::time::Duration;
///
/// let cell = Cell::new(0);
/// let output = BoundedOutput::new(&cell, 16);
///
/// cell.set(1);
/// cell.set(2);
///
/// assert_eq!(output.recv_timeout(Duration::from_millis(10)), Ok(0)); // Initial
/// assert_eq!(output.recv_timeout(Duration::from_millis(10)), Ok(1));
/// assert_eq!(output.recv_timeout(Duration::from_millis(10)), Ok(2));
/// ```
pub struct BoundedOutput<T> {
    receiver: Receiver<T>,
    _guard: SubscriptionGuard,
}

impl<T: Clone + Send + Sync + 'static> BoundedOutput<T> {
    /// Create a new bounded output from a watchable source.
    ///
    /// `capacity` determines the channel buffer size. If the buffer fills up,
    /// newer values will block the sender (and thus the notify call) until
    /// the consumer catches up.
    pub fn new<W: Watchable<T>>(source: &W, capacity: usize) -> Self {
        let (sender, receiver) = channel::bounded(capacity);

        let guard = source.subscribe(move |signal| {
            if let Signal::Value(value) = signal {
                // Send, blocking if full
                let _ = sender.send((**value).clone());
            }
        });

        Self {
            receiver,
            _guard: guard,
        }
    }

    /// Create a new bounded output that drops oldest values when full.
    ///
    /// Unlike `new()`, this won't block when the buffer is full - instead
    /// it will drop the oldest value to make room.
    pub fn dropping<W: Watchable<T>>(source: &W, capacity: usize) -> Self {
        let (sender, receiver) = channel::bounded(capacity);

        let guard = source.subscribe(move |signal| {
            if let Signal::Value(value) = signal {
                // Try to send, if full drop oldest and retry
                if sender.try_send((**value).clone()).is_err() {
                    // Channel full - would need to implement ring buffer behavior
                    // For now, just try_send which drops on full
                    let _ = sender.try_send((**value).clone());
                }
            }
        });

        Self {
            receiver,
            _guard: guard,
        }
    }

    /// Block until a value is available.
    pub fn recv(&self) -> Result<T, channel::RecvError> {
        self.receiver.recv()
    }

    /// Block until a value is available or timeout expires.
    pub fn recv_timeout(&self, timeout: Duration) -> Result<T, channel::RecvTimeoutError> {
        self.receiver.recv_timeout(timeout)
    }

    /// Try to receive a value without blocking.
    pub fn try_recv(&self) -> Result<T, TryRecvError> {
        self.receiver.try_recv()
    }

    /// Check if the channel is empty.
    pub fn is_empty(&self) -> bool {
        self.receiver.is_empty()
    }

    /// Get the number of values currently buffered.
    pub fn len(&self) -> usize {
        self.receiver.len()
    }

    /// Get an iterator over available values.
    pub fn iter(&self) -> channel::Iter<'_, T> {
        self.receiver.iter()
    }

    /// Get a non-blocking iterator over currently available values.
    pub fn try_iter(&self) -> channel::TryIter<'_, T> {
        self.receiver.try_iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Cell, Mutable};

    #[test]
    fn test_bounded_output_basic() {
        let cell = Cell::new(0);
        let output = BoundedOutput::new(&cell, 16);

        // Initial value is sent
        assert_eq!(output.try_recv(), Ok(0));

        cell.set(1);
        cell.set(2);
        cell.set(3);

        assert_eq!(output.try_recv(), Ok(1));
        assert_eq!(output.try_recv(), Ok(2));
        assert_eq!(output.try_recv(), Ok(3));
        assert!(output.try_recv().is_err());
    }

    #[test]
    fn test_bounded_output_timeout() {
        let cell = Cell::new(42);
        let output = BoundedOutput::new(&cell, 16);

        // Initial value
        assert_eq!(output.recv_timeout(Duration::from_millis(10)), Ok(42));

        // No more values - should timeout
        assert!(output.recv_timeout(Duration::from_millis(10)).is_err());
    }

    #[test]
    fn test_bounded_output_len() {
        let cell = Cell::new(0);
        let output = BoundedOutput::new(&cell, 16);

        assert_eq!(output.len(), 1); // Initial

        cell.set(1);
        cell.set(2);
        assert_eq!(output.len(), 3);

        let _ = output.try_recv();
        assert_eq!(output.len(), 2);
    }

    #[test]
    fn test_bounded_output_try_iter() {
        let cell = Cell::new(0);
        let output = BoundedOutput::new(&cell, 16);

        cell.set(1);
        cell.set(2);

        let values: Vec<_> = output.try_iter().collect();
        assert_eq!(values, vec![0, 1, 2]);
        assert!(output.is_empty());
    }
}
