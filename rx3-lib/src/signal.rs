use std::sync::Arc;

/// A signal emitted by a reactive cell.
///
/// This unifies value emission, completion, and error handling into a single type.
/// Operators can pattern match to handle each case appropriately.
#[derive(Debug, Clone)]
pub enum Signal<T> {
    /// A new value was emitted.
    Value(T),
    /// The stream has completed - no more values will be emitted.
    Complete,
    /// An error occurred in the stream.
    Error(Arc<anyhow::Error>),
}

impl<T> Signal<T> {
    /// Create an error signal from any error type.
    pub fn error(err: impl Into<anyhow::Error>) -> Self {
        Signal::Error(Arc::new(err.into()))
    }

    /// Returns true if this is a Value signal.
    pub fn is_value(&self) -> bool {
        matches!(self, Signal::Value(_))
    }

    /// Returns true if this is a Complete signal.
    pub fn is_complete(&self) -> bool {
        matches!(self, Signal::Complete)
    }

    /// Returns true if this is an Error signal.
    pub fn is_error(&self) -> bool {
        matches!(self, Signal::Error(_))
    }

    /// Returns the value if this is a Value signal.
    pub fn value(&self) -> Option<&T> {
        match self {
            Signal::Value(v) => Some(v),
            _ => None,
        }
    }

    /// Returns the error if this is an Error signal.
    pub fn error_ref(&self) -> Option<&Arc<anyhow::Error>> {
        match self {
            Signal::Error(e) => Some(e),
            _ => None,
        }
    }

    /// Maps the value inside a Value signal.
    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> Signal<U> {
        match self {
            Signal::Value(v) => Signal::Value(f(v)),
            Signal::Complete => Signal::Complete,
            Signal::Error(e) => Signal::Error(e),
        }
    }

    /// Maps a reference to the value inside a Value signal.
    pub fn map_ref<U>(&self, f: impl FnOnce(&T) -> U) -> Signal<U> {
        match self {
            Signal::Value(v) => Signal::Value(f(v)),
            Signal::Complete => Signal::Complete,
            Signal::Error(e) => Signal::Error(e.clone()),
        }
    }
}
