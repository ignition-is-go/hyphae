//! Experimental local cells backed by incremental-rs (`incremental-runtime`).
//!
//! No graph, scheduler, or dependency propagation is implemented here. The
//! backend owns those mechanisms. This facade defines ownership, transaction
//! boundaries, post-stabilization effects and explicit thread-safe publication.
//!
//! Cells have no `get`: retain an [`Observer`] for local snapshot reads, or
//! explicitly [`Runtime::publish`] a [`Snapshot`] for other threads. Only
//! observed branches compute. Inputs and closures may contain `Rc`/`RefCell`.
//!
//! Transactions are explicit, never time windows. Distinct input writes share
//! a stabilization step. Each input contributes at most one unequal write per step,
//! preserving unequal occurrences in order (including A -> B -> A). Effects
//! run after each step; their writes wait for the next explicit transaction.
//! Failed transaction construction applies nothing. Evaluation or effect panic
//! poisons the runtime; rollback of already executed computation is not promised.
//!
//! ```
//! use hyphae::incremental::Runtime;
//! # fn main() -> Result<(), hyphae::incremental::Error> {
//! let mut rt = Runtime::new();
//! let a = rt.input(1_u64);
//! let b = rt.input(2_u64);
//! let sum = a.cell().map2(&b.cell(), |a, b| a.saturating_add(*b))?;
//! let observed = sum.observe();
//! rt.transaction(|tx| { tx.set(&a, 10)?; tx.set(&b, 20) })?;
//! assert_eq!(observed.read()?, 30);
//! # Ok(()) }
//! ```
//!
//! Stored values must not be mutated through shared interior state. Computation
//! closures must use declared dependencies; retaining observers inside the graph
//! can create cycles and hidden reads and is unsupported.
//!
//! Graph handles cannot cross threads:
//!
//! ```compile_fail
//! use hyphae::incremental::Runtime;
//! let rt = Runtime::new();
//! let cell = rt.input(1).cell();
//! std::thread::spawn(move || drop(cell));
//! ```
//!
//! Publication imposes `Send` only at the explicit boundary:
//!
//! ```compile_fail
//! use hyphae::incremental::Runtime;
//! let rt = Runtime::new();
//! let cell = rt.input(std::rc::Rc::new(1)).cell();
//! let _publication = rt.publish(&cell);
//! ```

use std::{
    any::Any,
    cell::RefCell,
    collections::VecDeque,
    fmt,
    panic::{AssertUnwindSafe, catch_unwind},
    rc::{Rc, Weak},
    sync::{Arc, mpsc},
};

use incremental_engine::{Incr, IncrState, SubscriptionToken, Var, WeakState};
use parking_lot::Mutex;
use rustc_hash::FxHashMap;

/// Equality describes state identity; occurrence values need distinct IDs.
pub trait Value: Clone + Eq + 'static {}
impl<T: Clone + Eq + 'static> Value for T {}

// The backend requires Debug, but debug-printing user values is not
// part of our contract. This transparent-in-behavior wrapper avoids imposing
// an unrelated public Debug bound on cells.
#[derive(Clone, Eq, PartialEq)]
struct Stored<T>(T);
impl<T> fmt::Debug for Stored<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<cell value>")
    }
}

type Result<T> = std::result::Result<T, Error>;
type Effect = Box<dyn FnOnce(&mut Transaction)>;
type Effects = Rc<RefCell<Vec<Effect>>>;
type Ingress = RefCell<dyn FnMut(&mut Transaction) -> bool>;

/// An invalid boundary operation or an unusable runtime.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    /// Inputs/dependencies must belong to one runtime.
    ForeignRuntime,
    /// The observer has not survived a stabilization, or its branch is invalid.
    Unavailable,
    /// A user computation/effect panicked. Create a new runtime.
    Poisoned,
}
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "incremental runtime: {self:?}")
    }
}
impl std::error::Error for Error {}

/// Local graph handle. Reading requires an explicit observation boundary.
#[derive(Clone, Debug)]
pub struct Cell<T: Value> {
    node: Incr<Stored<T>>,
    owner: Rc<()>,
}
impl<T: Value> Cell<T> {
    /// Attach a diagnostic label to the backend node.
    #[must_use]
    pub fn with_name(self, name: impl Into<String>) -> Self {
        self.node.set_graphviz_user_data(name.into());
        self
    }

    /// Declare a pure dependency. Effects belong in `Runtime::subscribe`.
    #[must_use]
    pub fn map<R: Value>(&self, mut f: impl FnMut(&T) -> R + 'static) -> Cell<R> {
        Cell {
            node: self.node.map(move |value| Stored(f(&value.0))),
            owner: self.owner.clone(),
        }
    }

    /// Declare two dependencies; foreign runtimes are rejected before mutation.
    /// # Errors
    /// Returns `ForeignRuntime` when the dependencies belong to different runtimes.
    pub fn map2<U: Value, R: Value>(
        &self,
        other: &Cell<U>,
        mut f: impl FnMut(&T, &U) -> R + 'static,
    ) -> Result<Cell<R>> {
        same_owner(&self.owner, &other.owner)?;
        Ok(Cell {
            node: self
                .node
                .map2(&other.node, move |a, b| Stored(f(&a.0, &b.0))),
            owner: self.owner.clone(),
        })
    }

    /// Switch dependencies using the backend's scoped dynamic `bind`.
    ///
    /// Use the supplied builder for branch-local nodes. Previously created
    /// cells from this runtime may also be selected. Handles to newly created
    /// nodes escaping a replaced branch may become unavailable.
    ///
    /// # Panics
    /// A branch from another runtime is a programming error: stabilization
    /// catches the panic and poisons the runtime, just like a panicking mapper.
    #[must_use]
    pub fn switch_map<R: Value>(
        &self,
        mut f: impl FnMut(&Builder, &T) -> Cell<R> + 'static,
    ) -> Cell<R> {
        let owner = self.owner.clone();
        let node = self.node.binds(move |state, value| {
            let builder = Builder {
                state: state.clone(),
                owner: owner.clone(),
            };
            let selected = f(&builder, &value.0);
            assert!(
                Rc::ptr_eq(&owner, &selected.owner),
                "foreign switch_map branch"
            );
            selected.node
        });
        Cell {
            node,
            owner: self.owner.clone(),
        }
    }

    /// Keep this output demanded. No work runs until a transaction stabilizes.
    #[must_use]
    pub fn observe(&self) -> Observer<T> {
        Observer {
            inner: self.node.observe(),
        }
    }
}

/// Capability to construct nodes inside a dynamic branch, not run transactions.
pub struct Builder {
    state: WeakState,
    owner: Rc<()>,
}
impl Builder {
    /// A constant allocated in the current backend branch scope.
    #[must_use]
    pub fn constant<T: Value>(&self, value: T) -> Cell<T> {
        Cell {
            node: self.state.constant(Stored(value)),
            owner: self.owner.clone(),
        }
    }
}

/// Writable local input; writes require a transaction.
#[derive(Clone, Debug)]
pub struct Input<T: Value> {
    var: Var<Stored<T>>,
    cell: Cell<T>,
    current: Rc<RefCell<T>>,
    key: Rc<()>,
}
impl<T: Value> Input<T> {
    /// A read-only graph dependency, without copying the retained value.
    #[must_use]
    pub fn cell(&self) -> Cell<T> {
        self.cell.clone()
    }
}

/// Explicit demand and access to the last stabilized local value.
#[derive(Clone)]
pub struct Observer<T: Value> {
    inner: incremental_engine::Observer<Stored<T>>,
}
impl<T: Value> Observer<T> {
    /// Export backend dependency structure without maintaining a second graph.
    #[must_use]
    pub fn graph_dot(&self) -> String {
        self.inner.save_dot_to_string()
    }

    /// Does not trigger evaluation. Before the first stabilization it fails.
    /// # Errors
    /// Returns `Unavailable` before stabilization or for an invalidated branch.
    pub fn read(&self) -> Result<T> {
        self.inner
            .try_get_value()
            .map(|value| value.0)
            .map_err(|_| Error::Unavailable)
    }
}

struct Write {
    key: Rc<()>,
    apply: Box<dyn FnOnce() -> bool>,
}

/// A staged write set. Constructed by the runtime or passed to an effect.
pub struct Transaction {
    owner: Rc<()>,
    writes: Vec<Write>,
}
impl Transaction {
    const fn new(owner: Rc<()>) -> Self {
        Self {
            owner,
            writes: Vec::new(),
        }
    }

    /// Stage a write. Equal applied values are suppressed before reaching the backend.
    /// # Errors
    /// Returns `ForeignRuntime` for an input from another runtime.
    pub fn set<T: Value>(&mut self, input: &Input<T>, value: T) -> Result<()> {
        same_owner(&self.owner, &input.cell.owner)?;
        let var = input.var.clone();
        let current = input.current.clone();
        self.writes.push(Write {
            key: input.key.clone(),
            apply: Box::new(move || {
                if *current.borrow() == value {
                    false
                } else {
                    var.set(Stored(value.clone()));
                    *current.borrow_mut() = value;
                    true
                }
            }),
        });
        Ok(())
    }
}

/// Keeps an effect alive. Drop cancels delivery, including already queued delivery.
pub struct Subscription {
    state: WeakState,
    token: SubscriptionToken,
    _observer: Box<dyn Any>,
    _callback: Rc<dyn Any>,
}
impl Drop for Subscription {
    fn drop(&mut self) {
        if let Some(state) = self.state.upgrade() {
            state.unsubscribe(self.token);
        }
    }
}

/// A thread-safe view of the last published value. Never evaluates the graph.
#[derive(Clone)]
pub struct Snapshot<T> {
    value: Arc<Mutex<Option<T>>>,
}
impl<T: Clone> Snapshot<T> {
    /// `None` until the publication has survived its first transaction.
    #[must_use]
    pub fn read(&self) -> Option<T> {
        self.value.lock().clone()
    }
}

/// Local publication lifetime. Snapshot clones do not keep graph computation alive.
pub struct Publication<T> {
    snapshot: Snapshot<T>,
    _subscription: Subscription,
}
impl<T: Clone> Publication<T> {
    /// Only this explicit handle may be sent/shared across threads.
    #[must_use]
    pub fn snapshot(&self) -> Snapshot<T> {
        self.snapshot.clone()
    }
}

/// Local ingress lifetime. Drop disconnects sender clones and releases the input.
pub struct InputBridge<T> {
    sender: InputSender<T>,
    _ingress: Rc<Ingress>,
}
impl<T> InputBridge<T> {
    /// Only this explicit endpoint may cross a thread boundary.
    #[must_use]
    pub fn sender(&self) -> InputSender<T> {
        self.sender.clone()
    }
}

/// Bounded thread-safe input endpoint. `try_send` exposes backpressure explicitly.
pub struct InputSender<T> {
    sender: mpsc::SyncSender<T>,
    runtime: std::sync::Weak<()>,
}
impl<T> Clone for InputSender<T> {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
            runtime: self.runtime.clone(),
        }
    }
}
impl<T> InputSender<T> {
    /// Enqueue without blocking. Disconnected/full queues return the original value.
    /// # Errors
    /// Returns the value in `Full` or `Disconnected` when it cannot be enqueued.
    pub fn try_send(&self, value: T) -> std::result::Result<(), mpsc::TrySendError<T>> {
        let Some(_runtime) = self.runtime.upgrade() else {
            return Err(mpsc::TrySendError::Disconnected(value));
        };
        self.sender.try_send(value)
    }
}

/// Thread-local runtime. No clocks, worker pools or automatic frame boundaries.
pub struct Runtime {
    state: IncrState,
    owner: Rc<()>,
    effects: Effects,
    feedback: Vec<Write>,
    ingress: Vec<Weak<Ingress>>,
    next_ingress: usize,
    bridge_lifetime: Option<Arc<()>>,
    poisoned: bool,
}
impl Default for Runtime {
    fn default() -> Self {
        Self::new()
    }
}
impl Runtime {
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: IncrState::new(),
            owner: Rc::new(()),
            effects: Rc::default(),
            feedback: Vec::new(),
            ingress: Vec::new(),
            next_ingress: 0,
            bridge_lifetime: None,
            poisoned: false,
        }
    }

    /// Create state local to this runtime. No `Send` or `Sync` bound is imposed.
    #[must_use]
    pub fn input<T: Value>(&self, value: T) -> Input<T> {
        let var = self.state.var(Stored(value.clone()));
        Input {
            cell: Cell {
                node: var.watch(),
                owner: self.owner.clone(),
            },
            var,
            current: Rc::new(RefCell::new(value)),
            key: Rc::new(()),
        }
    }

    /// Create an immutable root outside any dynamic scope.
    #[must_use]
    pub fn constant<T: Value>(&self, value: T) -> Cell<T> {
        Cell {
            node: self.state.constant(Stored(value)),
            owner: self.owner.clone(),
        }
    }

    /// Aggregate declared dependencies using the backend's fold node.
    /// # Errors
    /// Returns `ForeignRuntime` if any dependency belongs to another runtime.
    pub fn fold<T: Value, R: Value>(
        &self,
        inputs: &[Cell<T>],
        initial: R,
        mut f: impl FnMut(R, &T) -> R + 'static,
    ) -> Result<Cell<R>> {
        for input in inputs {
            same_owner(&self.owner, &input.owner)?;
        }
        let nodes = inputs.iter().map(|cell| cell.node.clone()).collect();
        let node = self.state.fold(nodes, Stored(initial), move |acc, value| {
            Stored(f(acc.0, &value.0))
        });
        Ok(Cell {
            node,
            owner: self.owner.clone(),
        })
    }

    /// Stage writes, stabilize, then publish effects. Empty transactions also
    /// initialize new observers and apply feedback from the preceding transaction.
    /// Repeated-input steps preserve occurrences; distinct inputs settle together.
    /// # Errors
    /// Returns construction errors without applying writes, or `Poisoned` after
    /// an evaluation/effect panic. Already applied steps are not rolled back.
    pub fn transaction(&mut self, f: impl FnOnce(&mut Transaction) -> Result<()>) -> Result<()> {
        if self.poisoned {
            return Err(Error::Poisoned);
        }
        let mut staged = Transaction::new(self.owner.clone());
        f(&mut staged)?;
        let mut writes = std::mem::take(&mut self.feedback);
        writes.append(&mut staged.writes);
        let result = catch_unwind(AssertUnwindSafe(|| -> Result<()> {
            let mut indexes = FxHashMap::<*const (), usize>::default();
            let mut pending: Vec<VecDeque<Write>> = Vec::new();
            for write in writes {
                let index = *indexes.entry(Rc::as_ptr(&write.key)).or_insert_with(|| {
                    let index = pending.len();
                    pending.push(VecDeque::new());
                    index
                });
                pending
                    .get_mut(index)
                    .ok_or(Error::Poisoned)?
                    .push_back(write);
            }
            let mut first = true;
            while first || !pending.is_empty() {
                first = false;
                for queue in &mut pending {
                    while let Some(write) = queue.pop_front() {
                        if (write.apply)() {
                            break;
                        }
                    }
                }
                pending.retain(|queue| !queue.is_empty());
                self.settle();
            }
            Ok(())
        }));
        if !matches!(result, Ok(Ok(()))) {
            self.poisoned = true;
            self.effects.borrow_mut().clear();
            return Err(Error::Poisoned);
        }
        Ok(())
    }

    fn settle(&mut self) {
        self.state.stabilise();
        let jobs = std::mem::take(&mut *self.effects.borrow_mut());
        let mut feedback = Transaction::new(self.owner.clone());
        for job in jobs {
            job(&mut feedback);
        }
        self.feedback.append(&mut feedback.writes);
    }

    /// Effects run outside incremental-rs stabilization. Feedback is staged for
    /// the next explicit transaction, never recursively evaluated.
    /// # Errors
    /// Returns `ForeignRuntime` for a cell from another runtime or `Poisoned`
    /// when the runtime cannot evaluate further.
    pub fn subscribe<T: Value>(
        &self,
        cell: &Cell<T>,
        callback: impl FnMut(&T, &mut Transaction) + 'static,
    ) -> Result<Subscription> {
        if self.poisoned {
            return Err(Error::Poisoned);
        }
        same_owner(&self.owner, &cell.owner)?;
        let observer = cell.node.observe();
        let callback = Rc::new(RefCell::new(callback));
        let weak_callback = Rc::downgrade(&callback);
        let effects = Rc::downgrade(&self.effects);
        let token = observer.subscribe(move |update| {
            if let Some(value) = update.value() {
                let value = value.0.clone();
                let callback = weak_callback.clone();
                if let Some(effects) = effects.upgrade() {
                    effects.borrow_mut().push(Box::new(move |tx| {
                        if let Some(callback) = callback.upgrade() {
                            (callback.borrow_mut())(&value, tx);
                        }
                    }));
                }
            }
        });
        Ok(Subscription {
            state: self.state.weak(),
            token,
            _observer: Box::new(observer),
            _callback: callback,
        })
    }

    /// Opt in to synchronized publication. Only this boundary requires `Send`.
    /// # Errors
    /// Returns `ForeignRuntime` for a cell from another runtime or `Poisoned`
    /// when the runtime cannot evaluate further.
    pub fn publish<T: Value + Send>(&self, cell: &Cell<T>) -> Result<Publication<T>> {
        let snapshot = Snapshot {
            value: Arc::new(Mutex::new(None)),
        };
        let output = snapshot.clone();
        let subscription = self.subscribe(cell, move |value, _| {
            *output.value.lock() = Some(value.clone());
        })?;
        Ok(Publication {
            snapshot,
            _subscription: subscription,
        })
    }

    /// Opt in to a bounded ingress queue; the caller schedules explicit draining.
    /// Retain the returned local bridge while its sender should remain connected.
    /// # Errors
    /// Returns `ForeignRuntime` for an input from another runtime or `Poisoned`
    /// when the runtime cannot evaluate further.
    pub fn input_bridge<T: Value + Send>(
        &mut self,
        input: &Input<T>,
        capacity: usize,
    ) -> Result<InputBridge<T>> {
        if self.poisoned {
            return Err(Error::Poisoned);
        }
        same_owner(&self.owner, &input.cell.owner)?;
        let (sender, receiver) = mpsc::sync_channel(capacity);
        let input = input.clone();
        let ingress: Rc<Ingress> = Rc::new(RefCell::new(move |tx: &mut Transaction| {
            let Ok(value) = receiver.try_recv() else {
                return false;
            };
            // This input was validated against the runtime when the bridge was created.
            tx.set(&input, value).is_ok()
        }));
        self.ingress.push(Rc::downgrade(&ingress));
        Ok(InputBridge {
            sender: InputSender {
                sender,
                runtime: Arc::downgrade(self.bridge_lifetime.get_or_insert_with(|| Arc::new(()))),
            },
            _ingress: ingress,
        })
    }

    /// Consume at most `limit` queued updates, round-robin across input bridges.
    /// Every accepted unequal value is retained as a transaction step if needed.
    /// # Errors
    /// Returns `Poisoned` if evaluation/effects failed or the runtime is unusable.
    pub fn drain_inputs(&mut self, limit: usize) -> Result<usize> {
        if self.poisoned {
            return Err(Error::Poisoned);
        }
        let mut tx = Transaction::new(self.owner.clone());
        self.ingress.retain(|ingress| ingress.strong_count() > 0);
        let mut count = 0_usize;
        let mut empty = 0_usize;
        while count < limit && empty < self.ingress.len() {
            let index = self
                .next_ingress
                .checked_rem(self.ingress.len())
                .unwrap_or_default();
            self.next_ingress = index.saturating_add(1);
            let received = self
                .ingress
                .get(index)
                .and_then(Weak::upgrade)
                .is_some_and(|ingress| (ingress.borrow_mut())(&mut tx));
            if received {
                count = count.saturating_add(1);
                empty = 0;
            } else {
                empty = empty.saturating_add(1);
            }
        }
        self.transaction(move |staged| {
            staged.writes = tx.writes;
            Ok(())
        })?;
        Ok(count)
    }

    /// Backend work counters; no duplicate graph or graph census is maintained.
    #[must_use]
    pub fn stats(&self) -> incremental_engine::Stats {
        self.state.stats()
    }
}

fn same_owner(a: &Rc<()>, b: &Rc<()>) -> Result<()> {
    if Rc::ptr_eq(a, b) {
        Ok(())
    } else {
        Err(Error::ForeignRuntime)
    }
}

#[cfg(test)]
mod tests;
