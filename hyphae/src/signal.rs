use std::sync::Arc;

/// A signal emitted by a reactive cell.
///
/// This unifies value emission, completion, and error handling into a single type.
/// Operators can pattern match to handle each case appropriately.
///
/// Values are wrapped in `Arc<T>` for zero-copy forwarding through operator chains.
/// Cloning a Signal only bumps reference counts, never deep-copies the value.
#[derive(Debug, Clone)]
pub enum Signal<T> {
    /// A new value was emitted (wrapped in Arc for cheap cloning).
    Value(Arc<T>),
    /// The stream has completed - no more values will be emitted.
    Complete,
    /// An error occurred in the stream.
    Error(Arc<anyhow::Error>),
}

impl<T> Signal<T> {
    /// Create a value signal, wrapping in Arc.
    pub fn value(v: T) -> Self {
        Self::Value(Arc::new(v))
    }

    /// Create a value signal from an existing Arc.
    pub const fn value_arc(v: Arc<T>) -> Self {
        Self::Value(v)
    }

    /// Create an error signal from any error type.
    pub fn error(err: impl Into<anyhow::Error>) -> Self {
        Self::Error(Arc::new(err.into()))
    }

    /// Returns true if this is a Value signal.
    #[must_use]
    pub const fn is_value(&self) -> bool {
        matches!(self, Self::Value(_))
    }

    /// Returns true if this is a Complete signal.
    #[must_use]
    pub const fn is_complete(&self) -> bool {
        matches!(self, Self::Complete)
    }

    /// Returns true if this is an Error signal.
    #[must_use]
    pub const fn is_error(&self) -> bool {
        matches!(self, Self::Error(_))
    }

    /// Returns a reference to the value if this is a Value signal.
    #[must_use]
    pub fn value_ref(&self) -> Option<&T> {
        match self {
            Self::Value(v) => Some(v.as_ref()),
            _ => None,
        }
    }

    /// Returns the Arc if this is a Value signal.
    #[must_use]
    pub const fn arc(&self) -> Option<&Arc<T>> {
        match self {
            Self::Value(v) => Some(v),
            _ => None,
        }
    }

    /// Returns the error if this is an Error signal.
    #[must_use]
    pub const fn error_ref(&self) -> Option<&Arc<anyhow::Error>> {
        match self {
            Self::Error(e) => Some(e),
            _ => None,
        }
    }
}

impl<T: Clone> Signal<T> {
    /// Maps the value inside a Value signal.
    /// The mapper receives a reference and returns a new value.
    pub fn map<U>(&self, f: impl FnOnce(&T) -> U) -> Signal<U> {
        match self {
            Self::Value(v) => Signal::value(f(v.as_ref())),
            Self::Complete => Signal::Complete,
            Self::Error(e) => Signal::Error(e.clone()),
        }
    }
}
