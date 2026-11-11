//! Experimental, single-owner incremental evaluator (`local-runtime` feature).
//!
//! Inputs change only through explicit [`Runtime::transaction`] calls. Each
//! evaluation step applies at most one unequal write per input, settles all
//! demanded outputs, and then delivers observers. Repeated unequal writes to an
//! input form ordered steps; writes to distinct inputs may share a step. `Eq`
//! suppresses equal values throughout the graph. There is no wall-clock window.
//!
//! Operator closures must describe computation from their declared inputs.
//! External effects belong in subscriptions. Subscription feedback is queued
//! for the next explicit transaction, never evaluated reentrantly. Branches
//! created by `switch_map` have runtime-tracked scopes; handles escaping an old
//! scope become invalid when that branch is replaced.
//!
//! Values and closures require `Send`, but not `Sync`. One owner mutates graph
//! state without per-value locks. Handles can move between threads; reading or
//! evaluating requires the owning runtime. This is not an automatic parallel
//! scheduler and is not API-compatible with the existing push runtime.
//!
//! ```
//! use hyphae::local::Runtime;
//! # fn main() -> Result<(), hyphae::local::Error> {
//! let mut rt = Runtime::new();
//! let a = rt.input(1_u64)?;
//! let b = rt.input(2_u64)?;
//! let sum = rt.map2(&a.cell(), &b.cell(), |a, b| a.saturating_add(*b))?;
//! let observed = rt.observe(&sum)?;
//! rt.transaction(|tx| { tx.set(&a, 10)?; tx.set(&b, 20) })?;
//! assert_eq!(observed.get(&rt)?, 30);
//! # Ok(()) }
//! ```

//! Derived cells cannot be written as inputs:
//!
//! ```compile_fail
//! use hyphae::local::Runtime;
//! fn example() -> Result<(), hyphae::local::Error> {
//! let mut rt = Runtime::new();
//! let derived = rt.constant(1_u64)?;
//! rt.transaction(|tx| tx.set(&derived, 2))?;
//! Ok(())
//! }
//! ```
//!
//! Equality is required, rather than an optional operator:
//!
//! ```compile_fail
//! use hyphae::local::Runtime;
//! let mut rt = Runtime::new();
//! let _ = rt.input(0.0_f64); // Choose an explicit total-equality wrapper.
//! ```
//!
//! A branch factory constructs dependencies; it cannot start a transaction:
//!
//! ```compile_fail
//! use hyphae::local::{Error, Runtime};
//! fn example() -> Result<(), Error> {
//!     let mut rt = Runtime::new();
//!     let selector = rt.input(false)?;
//!     let _ = rt.switch_map(&selector.cell(), |builder, _| {
//!         builder.transaction(|_| Ok(()))?;
//!         builder.constant(0_u64)
//!     })?;
//!     Ok(())
//! }
//! ```

use std::{
    any::Any,
    fmt,
    marker::PhantomData,
    sync::{Arc, Weak},
};

use parking_lot::Mutex;

mod body;
mod runtime;

pub use runtime::Runtime;

/// Values compare by meaning. Give distinct occurrences distinct equality keys.
/// Do not mutate stored values through interior mutability or shared external state.
pub trait Value: Clone + Eq + Send + 'static {}
impl<T: Clone + Eq + Send + 'static> Value for T {}

type ErasedValue = Box<dyn Any + Send>;
type RetireQueue = Mutex<Vec<Retired>>;
type Result<T> = std::result::Result<T, Error>;

/// Invalid operations are explicit; evaluation failure poisons that runtime.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    ForeignRuntime,
    InvalidHandle,
    InvalidScope,
    Unsettled,
    TypeMismatch,
    NotInput,
    Cycle,
    Busy,
    Poisoned,
    EpochExhausted,
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "local runtime: {self:?}")
    }
}
impl std::error::Error for Error {}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct Id {
    slot: usize,
    generation: u64,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ScopeId {
    slot: usize,
    generation: u64,
}

enum Retired {
    Node(Id),
    Watch(u64),
}
struct Lease {
    id: Id,
    owner: Arc<()>,
    retired: Weak<RetireQueue>,
}
impl Drop for Lease {
    fn drop(&mut self) {
        if let Some(queue) = self.retired.upgrade() {
            queue.lock().push(Retired::Node(self.id));
        }
    }
}
struct WatchLease {
    id: u64,
    retired: Weak<RetireQueue>,
}
impl Drop for WatchLease {
    fn drop(&mut self) {
        if let Some(queue) = self.retired.upgrade() {
            queue.lock().push(Retired::Watch(self.id));
        }
    }
}

/// Typed graph handle. It does not expose a hidden, untracked `get` operation.
pub struct Cell<T: Value> {
    lease: Arc<Lease>,
    marker: PhantomData<fn() -> T>,
}
impl<T: Value> Clone for Cell<T> {
    fn clone(&self) -> Self {
        Self {
            lease: self.lease.clone(),
            marker: PhantomData,
        }
    }
}
impl<T: Value> fmt::Debug for Cell<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Cell").field(&self.lease.id).finish()
    }
}

/// Writable input; mutation requires an explicit transaction.
#[derive(Clone, Debug)]
pub struct Input<T: Value> {
    cell: Cell<T>,
}
impl<T: Value> Input<T> {
    #[must_use]
    pub fn cell(&self) -> Cell<T> {
        self.cell.clone()
    }
}

/// Demands an output. The last clone's drop releases that demand at the next boundary.
#[derive(Clone)]
pub struct Observer<T: Value> {
    cell: Cell<T>,
    _watch: Arc<WatchLease>,
}
impl<T: Value> Observer<T> {
    /// Read the settled value; returns `Unsettled` before its first evaluation.
    /// # Errors
    /// Returns an error for a foreign runtime, invalid branch, or unsettled output.
    pub fn get(&self, rt: &Runtime) -> Result<T> {
        rt.read(&self.cell)
    }
}

struct Write {
    input: Arc<Lease>,
    value: ErasedValue,
}

/// Input batch, also supplied to subscriptions for next-turn feedback.
pub struct Transaction {
    owner: Arc<()>,
    writes: Vec<Write>,
}
impl Transaction {
    /// Queue a value. Equal values are suppressed by the evaluator, not overwritten here.
    /// # Errors
    /// Returns `ForeignRuntime` if the input belongs to another runtime.
    pub fn set<T: Value>(&mut self, input: &Input<T>, value: T) -> Result<()> {
        if !Arc::ptr_eq(&self.owner, &input.cell.lease.owner) {
            return Err(Error::ForeignRuntime);
        }
        self.writes.push(Write {
            input: input.cell.lease.clone(),
            value: Box::new(value),
        });
        Ok(())
    }
}

/// Construction-only context for a dynamic branch: no mutation, reads or observation.
pub struct Builder<'a> {
    rt: &'a mut Runtime,
}
impl Builder<'_> {
    /// # Errors
    /// Returns an error for foreign or invalid dependencies or branch scope.
    pub fn constant<T: Value>(&mut self, value: T) -> Result<Cell<T>> {
        self.rt.make_constant(value)
    }
    /// # Errors
    /// Returns an error for foreign or invalid dependencies or branch scope.
    pub fn map<T: Value, U: Value>(
        &mut self,
        input: &Cell<T>,
        f: impl Fn(&T) -> U + Send + 'static,
    ) -> Result<Cell<U>> {
        self.rt.make_map(input, f)
    }
    /// # Errors
    /// Returns an error for foreign or invalid dependencies or branch scope.
    pub fn map2<A: Value, B: Value, U: Value>(
        &mut self,
        a: &Cell<A>,
        b: &Cell<B>,
        f: impl Fn(&A, &B) -> U + Send + 'static,
    ) -> Result<Cell<U>> {
        self.rt.make_map2(a, b, f)
    }
    /// # Errors
    /// Returns an error for foreign or invalid dependencies or branch scope.
    pub fn fold<T: Value, U: Value>(
        &mut self,
        inputs: &[Cell<T>],
        initial: U,
        f: impl Fn(U, &T) -> U + Send + 'static,
    ) -> Result<Cell<U>> {
        self.rt.make_fold(inputs, initial, f)
    }
    /// Fold distinct demanded input values into branch-owned state.
    /// # Errors
    /// Returns an error for foreign or invalid dependencies or branch scope.
    pub fn scan<T: Value, U: Value>(
        &mut self,
        input: &Cell<T>,
        initial: U,
        f: impl FnMut(&U, &T) -> U + Send + 'static,
    ) -> Result<Cell<U>> {
        self.rt.make_scan(input, initial, f)
    }
    /// # Errors
    /// Returns an error for foreign or invalid dependencies or branch scope.
    pub fn switch_map<T: Value, U: Value>(
        &mut self,
        input: &Cell<T>,
        f: impl Fn(&mut Builder<'_>, &T) -> Result<Cell<U>> + Send + 'static,
    ) -> Result<Cell<U>> {
        self.rt.make_switch(input, f)
    }
}

/// Cumulative counters; no profiler or global census is installed.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Stats {
    pub live_nodes: usize,
    pub live_scopes: usize,
    pub allocated_nodes: u64,
    pub reclaimed_nodes: u64,
    pub evaluations: u64,
    pub equality_cutoffs: u64,
    pub evaluation_steps: u64,
    pub notifications: u64,
}

#[cfg(test)]
mod tests;
