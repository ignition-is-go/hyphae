use crate::transaction::TxContext;
use std::sync::Arc;

/// A signal emitted by a reactive cell.
///
/// This unifies value emission, completion, and error handling into a single type.
/// Operators can pattern match to handle each case appropriately.
///
/// Values are wrapped in `Arc<T>` for zero-copy forwarding through operator chains.
/// Cloning a Signal only bumps reference counts, never deep-copies the value.
///
/// Signals can optionally carry a transaction context for glitch-free propagation.
/// When a transaction context is present, derived cells will wait until they receive
/// all expected updates before firing.
#[derive(Debug, Clone)]
pub enum Signal<T> {
    /// A new value was emitted (wrapped in Arc for cheap cloning).
    /// The optional TxContext enables glitch-free propagation.
    Value(Arc<T>, Option<TxContext>),
    /// The stream has completed - no more values will be emitted.
    Complete,
    /// An error occurred in the stream.
    Error(Arc<anyhow::Error>),
}

impl<T> Signal<T> {
    /// Create a value signal, wrapping in Arc (no transaction context).
    pub fn value(v: T) -> Self {
        Signal::Value(Arc::new(v), None)
    }

    /// Create a value signal with a transaction context.
    pub fn value_tx(v: T, ctx: TxContext) -> Self {
        Signal::Value(Arc::new(v), Some(ctx))
    }

    /// Create a value signal from an existing Arc (no transaction context).
    pub fn value_arc(v: Arc<T>) -> Self {
        Signal::Value(v, None)
    }

    /// Create a value signal from an existing Arc with transaction context.
    pub fn value_arc_tx(v: Arc<T>, ctx: TxContext) -> Self {
        Signal::Value(v, Some(ctx))
    }

    /// Create an error signal from any error type.
    pub fn error(err: impl Into<anyhow::Error>) -> Self {
        Signal::Error(Arc::new(err.into()))
    }

    /// Returns true if this is a Value signal.
    pub fn is_value(&self) -> bool {
        matches!(self, Signal::Value(_, _))
    }

    /// Returns true if this is a Complete signal.
    pub fn is_complete(&self) -> bool {
        matches!(self, Signal::Complete)
    }

    /// Returns true if this is an Error signal.
    pub fn is_error(&self) -> bool {
        matches!(self, Signal::Error(_))
    }

    /// Returns a reference to the value if this is a Value signal.
    pub fn value_ref(&self) -> Option<&T> {
        match self {
            Signal::Value(v, _) => Some(v.as_ref()),
            _ => None,
        }
    }

    /// Returns the Arc if this is a Value signal.
    pub fn arc(&self) -> Option<&Arc<T>> {
        match self {
            Signal::Value(v, _) => Some(v),
            _ => None,
        }
    }

    /// Returns the transaction context if this is a Value signal with one.
    pub fn tx_context(&self) -> Option<&TxContext> {
        match self {
            Signal::Value(_, ctx) => ctx.as_ref(),
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

    /// Create a copy of this signal with a new transaction context.
    pub fn with_tx_context(&self, ctx: Option<TxContext>) -> Self
    where
        T: Clone,
    {
        match self {
            Signal::Value(v, _) => Signal::Value(v.clone(), ctx),
            Signal::Complete => Signal::Complete,
            Signal::Error(e) => Signal::Error(e.clone()),
        }
    }
}

impl<T: Clone> Signal<T> {
    /// Maps the value inside a Value signal.
    /// The mapper receives a reference and returns a new value.
    /// Preserves transaction context if present.
    pub fn map<U>(&self, f: impl FnOnce(&T) -> U) -> Signal<U> {
        match self {
            Signal::Value(v, ctx) => Signal::Value(Arc::new(f(v.as_ref())), ctx.clone()),
            Signal::Complete => Signal::Complete,
            Signal::Error(e) => Signal::Error(e.clone()),
        }
    }
}
