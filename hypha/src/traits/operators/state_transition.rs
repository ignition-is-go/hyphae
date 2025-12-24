use std::{
    collections::HashMap,
    hash::Hash,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use arc_swap::ArcSwap;

use super::Watchable;
use crate::{
    cell::{Cell, CellImmutable, CellMutable},
    signal::Signal,
};

/// Type alias for transition handler callbacks.
type TransitionFn<S> = Arc<dyn Fn(&S, &S) + Send + Sync>;
/// Type alias for state enter/exit callbacks.
type StateFn<S> = Arc<dyn Fn(&S) + Send + Sync>;
/// Type alias for guard condition callbacks.
type GuardFn<S> = Arc<dyn Fn(&S, &S) -> bool + Send + Sync>;

/// Builder for defining state machine transitions.
pub struct StateMachineBuilder<S> {
    transitions: HashMap<(S, S), TransitionFn<S>>,
    on_enter: HashMap<S, StateFn<S>>,
    on_exit: HashMap<S, StateFn<S>>,
    guards: HashMap<(S, S), GuardFn<S>>,
    on_any_enter: Vec<StateFn<S>>,
    on_invalid: Option<TransitionFn<S>>,
}

impl<S: Eq + Hash + Clone + Send + Sync + 'static> StateMachineBuilder<S> {
    fn new() -> Self {
        Self {
            transitions: HashMap::new(),
            on_enter: HashMap::new(),
            on_exit: HashMap::new(),
            guards: HashMap::new(),
            on_any_enter: Vec::new(),
            on_invalid: None,
        }
    }

    /// Define a valid transition from `from` to `to` with a handler.
    pub fn on<F>(&mut self, from: S, to: S, handler: F) -> &mut Self
    where
        F: Fn(&S, &S) + Send + Sync + 'static,
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
    /// Only emits values when a valid transition occurs. Invalid transitions are
    /// filtered out (or handled by on_invalid if specified).
    ///
    /// # Example
    ///
    /// ```
    /// use hypha::{Cell, Mutable, Gettable, StateTransitionExt};
    ///
    /// #[derive(Clone, PartialEq, Eq, Hash, Debug)]
    /// enum State { Idle, Loading, Ready, Error }
    ///
    /// let source = Cell::new(State::Idle);
    /// let sm = source.state_transition(|sm| {
    ///     sm.on(State::Idle, State::Loading, |_, _| println!("started"));
    ///     sm.on(State::Loading, State::Ready, |_, _| println!("done"));
    ///     sm.on(State::Loading, State::Error, |_, _| println!("failed"));
    /// });
    ///
    /// source.set(State::Loading); // Valid - emits Loading
    /// source.set(State::Ready);   // Valid - emits Ready
    /// source.set(State::Idle);    // Invalid - filtered out
    /// ```
    fn state_transition<F>(&self, configure: F) -> Cell<S, CellImmutable>
    where
        S: Clone + Eq + Hash + Send + Sync + 'static,
        F: FnOnce(&mut StateMachineBuilder<S>),
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

        let derived = Cell::<S, CellMutable>::new(self.get());

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

                        // Check if transition is defined
                        let is_valid = transitions.contains_key(&key);

                        if !is_valid {
                            // Invalid transition
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

                        // 2. transition handler
                        if let Some(trans_fn) = transitions.get(&key) {
                            trans_fn(&*current, &**next);
                        }

                        // 3. on_enter for next state
                        if let Some(enter_fn) = on_enter.get(&**next) {
                            enter_fn(&**next);
                        }

                        // 4. on_any handlers
                        for handler in on_any_enter.iter() {
                            handler(&**next);
                        }

                        // Update current state and emit
                        current_state.store(next.clone());
                        d.notify(Signal::Value(next.clone()));
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
            });
            sm.on(State::Loading, State::Ready, |_, _| {});
            sm.on(State::Loading, State::Error, |_, _| {});
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
    fn test_state_transition_invalid_filtered() {
        let source = Cell::new(State::Idle);
        let sm = source.state_transition(|sm| {
            sm.on(State::Idle, State::Loading, |_, _| {});
            sm.on(State::Loading, State::Ready, |_, _| {});
        });

        let emissions = Arc::new(AtomicU32::new(0));
        let e = emissions.clone();
        let _guard = sm.subscribe(move |_| {
            e.fetch_add(1, Ordering::SeqCst);
        });

        assert_eq!(emissions.load(Ordering::SeqCst), 1); // Initial

        // Invalid transition: Idle -> Ready (not defined)
        source.set(State::Ready);
        assert_eq!(emissions.load(Ordering::SeqCst), 1); // Not emitted

        // Invalid transition: Idle -> Error (not defined)
        source.set(State::Error);
        assert_eq!(emissions.load(Ordering::SeqCst), 1); // Not emitted

        // Valid transition
        source.set(State::Loading);
        assert_eq!(emissions.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn test_state_transition_on_enter_exit() {
        let source = Cell::new(State::Idle);
        let enter_count = Arc::new(AtomicU32::new(0));
        let exit_count = Arc::new(AtomicU32::new(0));

        let ec = enter_count.clone();
        let xc = exit_count.clone();
        let _sm = source.state_transition(|sm| {
            sm.on(State::Idle, State::Loading, |_, _| {});
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
            sm.on(State::Idle, State::Loading, |_, _| {});
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
            sm.on(State::Idle, State::Loading, |_, _| {});
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
        let _sm = source.state_transition(|sm| {
            sm.on(State::Idle, State::Loading, |_, _| {});
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
}
