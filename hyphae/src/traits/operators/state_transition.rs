use std::{
    collections::HashMap,
    hash::Hash,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use arc_swap::ArcSwap;

use super::{CellValue, Watchable};
use crate::{
    cell::{Cell, CellImmutable, CellMutable},
    signal::Signal,
};

/// Type alias for transition handler callbacks.
/// Returns a value that is emitted downstream for this transition.
type TransitionFn<S, R> = Arc<dyn Fn(&S, &S) -> R + Send + Sync>;
/// Type alias for state enter/exit callbacks.
type StateFn<S> = Arc<dyn Fn(&S) + Send + Sync>;
/// Type alias for guard condition callbacks.
type GuardFn<S> = Arc<dyn Fn(&S, &S) -> bool + Send + Sync>;
/// Type alias for invalid transition handler callbacks.
type InvalidFn<S> = Arc<dyn Fn(&S, &S) + Send + Sync>;

/// Builder for defining state machine transitions.
pub struct StateMachineBuilder<S, R> {
    transitions: HashMap<(S, S), TransitionFn<S, R>>,
    on_enter: HashMap<S, StateFn<S>>,
    on_exit: HashMap<S, StateFn<S>>,
    guards: HashMap<(S, S), GuardFn<S>>,
    on_any_enter: Vec<StateFn<S>>,
    on_invalid: Option<InvalidFn<S>>,
    default: Option<R>,
}

impl<S: Eq + Hash + CellValue, R: CellValue> StateMachineBuilder<S, R> {
    fn new() -> Self {
        Self {
            transitions: HashMap::new(),
            on_enter: HashMap::new(),
            on_exit: HashMap::new(),
            guards: HashMap::new(),
            on_any_enter: Vec::new(),
            on_invalid: None,
            default: None,
        }
    }

    /// Set the initial value of the output cell.
    /// If not called, `R::default()` is used.
    pub fn with_default(&mut self, value: R) -> &mut Self {
        self.default = Some(value);
        self
    }

    /// Define a valid transition from `from` to `to` with a handler.
    /// The handler receives (from_state, to_state) and returns a value
    /// that is emitted downstream.
    pub fn on<F>(&mut self, from: S, to: S, handler: F) -> &mut Self
    where
        F: Fn(&S, &S) -> R + Send + Sync + 'static,
    {
        self.transitions.insert((from, to), Arc::new(handler));
        self
    }

    /// Handler called when entering a specific state (from any valid transition).
    pub fn on_enter<F>(&mut self, state: S, handler: F) -> &mut Self
    where
        F: Fn(&S) + Send + Sync + 'static,
    {
        self.on_enter.insert(state, Arc::new(handler));
        self
    }

    /// Handler called when exiting a specific state (via any valid transition).
    pub fn on_exit<F>(&mut self, state: S, handler: F) -> &mut Self
    where
        F: Fn(&S) + Send + Sync + 'static,
    {
        self.on_exit.insert(state, Arc::new(handler));
        self
    }

    /// Handler called when entering any state.
    pub fn on_any<F>(&mut self, handler: F) -> &mut Self
    where
        F: Fn(&S) + Send + Sync + 'static,
    {
        self.on_any_enter.push(Arc::new(handler));
        self
    }

    /// Guard condition: transition only happens if predicate returns true.
    pub fn guard<F>(&mut self, from: S, to: S, predicate: F) -> &mut Self
    where
        F: Fn(&S, &S) -> bool + Send + Sync + 'static,
    {
        self.guards.insert((from, to), Arc::new(predicate));
        self
    }

    /// Handler called when an invalid transition is attempted.
    pub fn on_invalid<F>(&mut self, handler: F) -> &mut Self
    where
        F: Fn(&S, &S) + Send + Sync + 'static,
    {
        self.on_invalid = Some(Arc::new(handler));
        self
    }
}

pub trait StateTransitionExt<S>: Watchable<S> {
    /// State machine operator for defining valid transitions and transition handlers.
    ///
    /// Each transition handler returns a value of type `R` that is emitted downstream.
    /// The state machine tracks the source state `S` internally but emits `R`.
    /// The internal state always advances to match the upstream value, even for
    /// undefined transitions — only defined transitions produce output.
    /// Use `on_invalid` to observe undefined transitions without emitting.
    ///
    /// # Example
    ///
    /// ```
    /// use hyphae::{Cell, Mutable, Gettable, StateTransitionExt, FilterExt};
    ///
    /// #[derive(Clone, PartialEq, Eq, Hash, Debug)]
    /// enum State { Idle, Loading, Ready, Error }
    ///
    /// let source = Cell::new(State::Idle);
    /// let sm = source.state_transition(|sm| {
    ///     sm.on(State::Idle, State::Loading, |_, _| true);    // emit true
    ///     sm.on(State::Loading, State::Ready, |_, _| false);  // emit false
    ///     sm.on(State::Loading, State::Error, |_, _| false);  // emit false
    /// });
    ///
    /// // Filter to only react to specific transitions
    /// let triggers = sm.filter(|v| *v);
    /// ```
    #[track_caller]
    fn state_transition<R, F>(&self, configure: F) -> Cell<R, CellImmutable>
    where
        S: CellValue + Eq + Hash,
        R: CellValue + Default,
        F: FnOnce(&mut StateMachineBuilder<S, R>),
        Self: Clone + Send + Sync + 'static,
    {
        let mut builder = StateMachineBuilder::new();
        configure(&mut builder);

        let transitions = Arc::new(builder.transitions);
        let on_enter = Arc::new(builder.on_enter);
        let on_exit = Arc::new(builder.on_exit);
        let guards = Arc::new(builder.guards);
        let on_any_enter = Arc::new(builder.on_any_enter);
        let on_invalid = builder.on_invalid;

        let initial = builder.default.take().unwrap_or_default();
        let derived = Cell::<R, CellMutable>::new(initial);
        let derived = if let Some(name) = self.name() {
            derived.with_name(format!("{}::state_transition", name))
        } else {
            derived
        };

        let weak = derived.downgrade();
        let first = Arc::new(AtomicBool::new(true));
        let current_state: Arc<ArcSwap<S>> = Arc::new(ArcSwap::from_pointee(self.get()));

        let guard = self.subscribe(move |signal| {
            if let Some(d) = weak.upgrade() {
                match signal {
                    Signal::Value(next) => {
                        if first.swap(false, Ordering::SeqCst) {
                            return;
                        }

                        let current = current_state.load();
                        let key = ((**current).clone(), (**next).clone());

                        // Always advance state to track upstream reality.
                        // This ensures undefined transitions don't "stick"
                        // the state machine — only defined transitions
                        // produce output.
                        current_state.store(next.clone());

                        // Check if transition is defined
                        if !transitions.contains_key(&key) {
                            if let Some(ref handler) = on_invalid {
                                handler(&*current, &**next);
                            }
                            return;
                        }

                        // Check guard if defined
                        if let Some(guard_fn) = guards.get(&key)
                            && !guard_fn(&*current, &**next)
                        {
                            return; // Guard rejected
                        }

                        // Valid transition - execute handlers
                        // 1. on_exit for current state
                        if let Some(exit_fn) = on_exit.get(&*current) {
                            exit_fn(&*current);
                        }

                        // 2. transition handler — returns value to emit
                        let output = transitions
                            .get(&key)
                            .map(|trans_fn| trans_fn(&*current, &**next));

                        // 3. on_enter for next state
                        if let Some(enter_fn) = on_enter.get(&**next) {
                            enter_fn(&**next);
                        }

                        // 4. on_any handlers
                        for handler in on_any_enter.iter() {
                            handler(&**next);
                        }

                        // Emit handler's return value
                        if let Some(value) = output {
                            d.notify(Signal::Value(Arc::new(value)));
                        }
                    }
                    Signal::Complete => d.notify(Signal::Complete),
                    Signal::Error(e) => d.notify(Signal::Error(e.clone())),
                }
            }
        });
        derived.own(guard);

        derived.lock()
    }
}

impl<S, W: Watchable<S>> StateTransitionExt<S> for W {}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;
    use crate::Mutable;

    #[derive(Clone, PartialEq, Eq, Hash, Debug)]
    enum State {
        Idle,
        Loading,
        Ready,
        Error,
    }

    #[test]
    fn test_state_transition_valid() {
        let source = Cell::new(State::Idle);
        let transition_count = Arc::new(AtomicU32::new(0));

        let tc = transition_count.clone();
        let sm = source.state_transition(|sm| {
            sm.on(State::Idle, State::Loading, move |_, _| {
                tc.fetch_add(1, Ordering::SeqCst);
                true
            });
            sm.on(State::Loading, State::Ready, |_, _| true);
            sm.on(State::Loading, State::Error, |_, _| true);
        });

        let emissions = Arc::new(AtomicU32::new(0));
        let e = emissions.clone();
        let _guard = sm.subscribe(move |_| {
            e.fetch_add(1, Ordering::SeqCst);
        });

        assert_eq!(emissions.load(Ordering::SeqCst), 1); // Initial

        // Valid transition
        source.set(State::Loading);
        assert_eq!(emissions.load(Ordering::SeqCst), 2);
        assert_eq!(transition_count.load(Ordering::SeqCst), 1);

        // Another valid transition
        source.set(State::Ready);
        assert_eq!(emissions.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn test_state_transition_undefined_advances_state() {
        let source = Cell::new(State::Idle);
        let sm = source.state_transition(|sm| {
            sm.on(State::Idle, State::Loading, |_, _| true);
            sm.on(State::Loading, State::Ready, |_, _| true);
        });

        let emissions = Arc::new(AtomicU32::new(0));
        let e = emissions.clone();
        let _guard = sm.subscribe(move |_| {
            e.fetch_add(1, Ordering::SeqCst);
        });

        assert_eq!(emissions.load(Ordering::SeqCst), 1); // Initial

        // Undefined: Idle -> Ready — state advances to Ready, no emission
        source.set(State::Ready);
        assert_eq!(emissions.load(Ordering::SeqCst), 1);

        // Undefined: Ready -> Error — state advances to Error, no emission
        source.set(State::Error);
        assert_eq!(emissions.load(Ordering::SeqCst), 1);

        // Undefined: Error -> Loading — state advances to Loading, no emission
        // (state machine tracks actual upstream state)
        source.set(State::Loading);
        assert_eq!(emissions.load(Ordering::SeqCst), 1);

        // Defined: Loading -> Ready — emits!
        source.set(State::Ready);
        assert_eq!(emissions.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn test_state_transition_on_enter_exit() {
        let source = Cell::new(State::Idle);
        let enter_count = Arc::new(AtomicU32::new(0));
        let exit_count = Arc::new(AtomicU32::new(0));

        let ec = enter_count.clone();
        let xc = exit_count.clone();
        let _sm: Cell<bool, _> = source.state_transition(|sm| {
            sm.on(State::Idle, State::Loading, |_, _| true);
            sm.on_exit(State::Idle, move |_| {
                xc.fetch_add(1, Ordering::SeqCst);
            });
            sm.on_enter(State::Loading, move |_| {
                ec.fetch_add(1, Ordering::SeqCst);
            });
        });

        source.set(State::Loading);
        assert_eq!(exit_count.load(Ordering::SeqCst), 1);
        assert_eq!(enter_count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn test_state_transition_guard() {
        let source = Cell::new(State::Idle);
        let allow = Arc::new(AtomicBool::new(false));

        let a = allow.clone();
        let sm = source.state_transition(|sm| {
            sm.on(State::Idle, State::Loading, |_, _| true);
            sm.guard(State::Idle, State::Loading, move |_, _| {
                a.load(Ordering::SeqCst)
            });
        });

        let emissions = Arc::new(AtomicU32::new(0));
        let e = emissions.clone();
        let _guard = sm.subscribe(move |_| {
            e.fetch_add(1, Ordering::SeqCst);
        });

        // Guard rejects
        source.set(State::Loading);
        assert_eq!(emissions.load(Ordering::SeqCst), 1); // Still 1

        // Reset and allow
        source.set(State::Idle); // This is invalid too, but reset source
        allow.store(true, Ordering::SeqCst);

        // Create fresh cell since current state might be Loading in sm
        let source2 = Cell::new(State::Idle);
        let a2 = allow.clone();
        let sm2 = source2.state_transition(|sm| {
            sm.on(State::Idle, State::Loading, |_, _| true);
            sm.guard(State::Idle, State::Loading, move |_, _| {
                a2.load(Ordering::SeqCst)
            });
        });

        let emissions2 = Arc::new(AtomicU32::new(0));
        let e2 = emissions2.clone();
        let _guard2 = sm2.subscribe(move |_| {
            e2.fetch_add(1, Ordering::SeqCst);
        });

        source2.set(State::Loading);
        assert_eq!(emissions2.load(Ordering::SeqCst), 2); // Now passes
    }

    #[test]
    fn test_state_transition_on_invalid() {
        let source = Cell::new(State::Idle);
        let invalid_count = Arc::new(AtomicU32::new(0));

        let ic = invalid_count.clone();
        let _sm: Cell<bool, _> = source.state_transition(|sm| {
            sm.on(State::Idle, State::Loading, |_, _| true);
            sm.on_invalid(move |_, _| {
                ic.fetch_add(1, Ordering::SeqCst);
            });
        });

        // Invalid transition
        source.set(State::Ready);
        assert_eq!(invalid_count.load(Ordering::SeqCst), 1);

        // Another invalid
        source.set(State::Error);
        assert_eq!(invalid_count.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn test_state_transition_selective_emit() {
        use crate::{FilterExt, Gettable, Pipeline};

        let source = Cell::new(State::Idle);
        let sm = source.state_transition(|sm| {
            sm.on(State::Idle, State::Loading, |_, _| true);
            sm.on(State::Loading, State::Ready, |_, _| false);
            sm.on(State::Ready, State::Idle, |_, _| false);
        });
        let triggers = sm.filter(|v| *v).materialize();

        let emission_count = Arc::new(AtomicU32::new(0));
        let ec = emission_count.clone();
        let _guard = triggers.subscribe(move |_| {
            ec.fetch_add(1, Ordering::SeqCst);
        });

        assert_eq!(emission_count.load(Ordering::SeqCst), 1); // Initial (false)

        source.set(State::Loading); // true - emits
        assert_eq!(emission_count.load(Ordering::SeqCst), 2);

        source.set(State::Ready); // false - filtered
        assert_eq!(emission_count.load(Ordering::SeqCst), 2);

        source.set(State::Idle); // false - filtered
        assert_eq!(emission_count.load(Ordering::SeqCst), 2);

        source.set(State::Loading); // true again - emits
        assert_eq!(emission_count.load(Ordering::SeqCst), 3);
        assert!(triggers.get());
    }
}
