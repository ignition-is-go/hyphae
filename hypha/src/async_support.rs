//! Async/await integration for hypha.
//!
//! This module provides async Stream adapters for Cells, enabling use with
//! async runtimes like tokio, async-std, or smol.
//!
//! Enable with the `async` feature flag.

use futures_core::Stream;

use crate::signal::Signal;
use crate::subscription::SubscriptionGuard;
use crate::traits::Watchable;

/// A Stream adapter for watching Cell values.
///
/// Created by [`AsyncWatchableExt::to_stream`].
pub struct CellStream<T: 'static> {
    receiver: flume::r#async::RecvStream<'static, T>,
    _guard: SubscriptionGuard,
}

impl<T: 'static> Stream for CellStream<T> {
    type Item = T;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        std::pin::Pin::new(&mut self.receiver).poll_next(cx)
    }
}

/// Extension trait for async operations on Watchable types.
pub trait AsyncWatchableExt<T>: Watchable<T> {
    /// Convert this watchable into an async Stream.
    ///
    /// The stream will emit the current value immediately, then emit
    /// each subsequent value as it changes.
    ///
    /// # Example
    ///
    /// ```ignore
    /// use hypha::{Cell, Mutable, AsyncWatchableExt};
    /// use futures::StreamExt;
    ///
    /// async fn example() {
    ///     let cell = Cell::new(0);
    ///     let mut stream = cell.to_stream();
    ///
    ///     // First value is current
    ///     assert_eq!(stream.next().await, Some(0));
    ///
    ///     cell.set(1);
    ///     assert_eq!(stream.next().await, Some(1));
    /// }
    /// ```
    fn to_stream(&self) -> CellStream<T>
    where
        T: Clone + Send + Sync + 'static,
        Self: Clone + Send + Sync + 'static,
    {
        let (sender, receiver) = flume::unbounded();

        let guard = self.subscribe(move |signal| {
            if let Signal::Value(value) = signal {
                let _ = sender.send((**value).clone());
            }
        });

        CellStream {
            receiver: receiver.into_stream(),
            _guard: guard,
        }
    }

    /// Convert this watchable into a bounded async Stream.
    ///
    /// If the consumer is slower than the producer and the buffer fills,
    /// the sender will block (applying backpressure to notify calls).
    fn to_stream_bounded(&self, capacity: usize) -> CellStream<T>
    where
        T: Clone + Send + Sync + 'static,
        Self: Clone + Send + Sync + 'static,
    {
        let (sender, receiver) = flume::bounded(capacity);

        let guard = self.subscribe(move |signal| {
            if let Signal::Value(value) = signal {
                let _ = sender.send((**value).clone());
            }
        });

        CellStream {
            receiver: receiver.into_stream(),
            _guard: guard,
        }
    }
}

impl<T, W: Watchable<T>> AsyncWatchableExt<T> for W {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Cell, Mutable};
    use futures_core::Stream;
    use std::pin::Pin;
    use std::task::{Context, Poll, Waker};

    // Simple test waker that does nothing
    fn noop_waker() -> Waker {
        use std::sync::Arc;
        use std::task::Wake;

        struct NoopWaker;
        impl Wake for NoopWaker {
            fn wake(self: Arc<Self>) {}
        }

        Arc::new(NoopWaker).into()
    }

    #[test]
    fn test_to_stream_immediate_value() {
        let cell = Cell::new(42);
        let mut stream = cell.to_stream();

        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        // First poll should return the initial value
        match Pin::new(&mut stream).poll_next(&mut cx) {
            Poll::Ready(Some(value)) => assert_eq!(value, 42),
            _ => panic!("Expected Ready with initial value"),
        }
    }

    #[test]
    fn test_to_stream_updates() {
        let cell = Cell::new(0);
        let mut stream = cell.to_stream();

        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        // Consume initial value
        let _ = Pin::new(&mut stream).poll_next(&mut cx);

        // Set new value
        cell.set(1);

        // Should get the new value
        match Pin::new(&mut stream).poll_next(&mut cx) {
            Poll::Ready(Some(value)) => assert_eq!(value, 1),
            _ => panic!("Expected Ready with new value"),
        }
    }

    #[test]
    fn test_to_stream_bounded() {
        let cell = Cell::new(0);
        let mut stream = cell.to_stream_bounded(2);

        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        // Consume initial value
        let _ = Pin::new(&mut stream).poll_next(&mut cx);

        // Set values
        cell.set(1);
        cell.set(2);

        // Should get both values
        match Pin::new(&mut stream).poll_next(&mut cx) {
            Poll::Ready(Some(value)) => assert_eq!(value, 1),
            _ => panic!("Expected Ready"),
        }
        match Pin::new(&mut stream).poll_next(&mut cx) {
            Poll::Ready(Some(value)) => assert_eq!(value, 2),
            _ => panic!("Expected Ready"),
        }
    }
}
