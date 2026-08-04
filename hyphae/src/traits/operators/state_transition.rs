use std::{
    collections::HashMap,
    hash::Hash,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use super::CellValue;
use crate::{
    pipeline::{Definite, Pipeline, PipelineInstall, PipelineSeed},
    signal::Signal,
    subscription::SubscriptionGuard,
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

pub struct StateTransitionPipeline<P, S, R> {
    source: P,
    transitions: Arc<HashMap<(S, S), TransitionFn<S, R>>>,
    on_enter: Arc<HashMap<S, StateFn<S>>>,
    on_exit: Arc<HashMap<S, StateFn<S>>>,
    guards: Arc<HashMap<(S, S), GuardFn<S>>>,
    on_any_enter: Arc<Vec<StateFn<S>>>,
    on_invalid: Option<InvalidFn<S>>,
    initial: R,
}

impl<P, S, R> PipelineInstall<R> for StateTransitionPipeline<P, S, R>
where
    P: PipelineInstall<S> + PipelineSeed<S>,
    S: CellValue + Eq + Hash,
    R: CellValue,
{
    fn install(&self, callback: Arc<dyn Fn(&Signal<R>) + Send + Sync>) -> SubscriptionGuard {
        let transitions = self.transitions.clone();
        let on_enter = self.on_enter.clone();
        let on_exit = self.on_exit.clone();
        let guards = self.guards.clone();
        let on_any_enter = self.on_any_enter.clone();
        let on_invalid = self.on_invalid.clone();
        let first = AtomicBool::new(true);
        let current_state = Arc::new(Mutex::new(self.source.seed()));

        self.source.install(Arc::new(move |signal| match signal {
            Signal::Value(next) => {
                if first.swap(false, Ordering::SeqCst) {
                    return;
                }
                let current = {
                    let mut guard = current_state.lock().expect("state_transition poisoned");
                    let previous = guard.clone();
                    *guard = next.as_ref().clone();
                    previous
                };
                let key = (current.clone(), next.as_ref().clone());
                if !transitions.contains_key(&key) {
                    if let Some(handler) = &on_invalid {
                        handler(&current, next);
                    }
                    return;
                }
                if let Some(guard) = guards.get(&key)
                    && !guard(&current, next)
                {
                    return;
                }
                if let Some(handler) = on_exit.get(&current) {
                    handler(&current);
                }
                let output = transitions.get(&key).map(|handler| handler(&current, next));
                if let Some(handler) = on_enter.get(next.as_ref()) {
                    handler(next);
                }
                for handler in on_any_enter.iter() {
                    handler(next);
                }
                if let Some(value) = output {
                    callback(&Signal::value(value));
                }
            }
            Signal::Complete => callback(&Signal::Complete),
            Signal::Error(error) => callback(&Signal::Error(error.clone())),
        }))
    }
}

impl<P, S, R> PipelineSeed<R> for StateTransitionPipeline<P, S, R>
where
    P: PipelineInstall<S> + PipelineSeed<S>,
    S: CellValue + Eq + Hash,
    R: CellValue,
{
    fn seed(&self) -> R {
        self.initial.clone()
    }
}

#[allow(private_bounds)]
impl<P, S, R> Pipeline<R, Definite> for StateTransitionPipeline<P, S, R>
where
    P: Pipeline<S, Definite> + PipelineSeed<S>,
    S: CellValue + Eq + Hash,
    R: CellValue,
{
}

#[allow(private_bounds)]
pub trait StateTransitionExt<S: CellValue + Eq + Hash>:
    Pipeline<S, Definite> + PipelineSeed<S>
{
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
    fn state_transition<R, F>(self, configure: F) -> impl crate::Materialize<R, Definite>
    where
        S: CellValue + Eq + Hash,
        R: CellValue + Default,
        F: FnOnce(&mut StateMachineBuilder<S, R>),
    {
        let mut builder = StateMachineBuilder::new();
        configure(&mut builder);

        let initial = builder.default.take().unwrap_or_default();
        StateTransitionPipeline {
            source: self,
            transitions: Arc::new(builder.transitions),
            on_enter: Arc::new(builder.on_enter),
            on_exit: Arc::new(builder.on_exit),
            guards: Arc::new(builder.guards),
            on_any_enter: Arc::new(builder.on_any_enter),
            on_invalid: builder.on_invalid,
            initial,
        }
    }
}

impl<S, P> StateTransitionExt<S> for P
where
    S: CellValue + Eq + Hash,
    P: Pipeline<S, Definite> + PipelineSeed<S>,
{
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;
    use crate::{Cell, Materialize, Mutable, traits::Watchable};

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
        let sm = source
            .clone()
            .state_transition(|sm| {
                sm.on(State::Idle, State::Loading, move |_, _| {
                    tc.fetch_add(1, Ordering::SeqCst);
                    true
                });
                sm.on(State::Loading, State::Ready, |_, _| true);
                sm.on(State::Loading, State::Error, |_, _| true);
            })
            .materialize();

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
        let sm = source
            .clone()
            .state_transition(|sm| {
                sm.on(State::Idle, State::Loading, |_, _| true);
                sm.on(State::Loading, State::Ready, |_, _| true);
            })
            .materialize();

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
        let _sm: Cell<bool, _> = source
            .clone()
            .state_transition(|sm| {
                sm.on(State::Idle, State::Loading, |_, _| true);
                sm.on_exit(State::Idle, move |_| {
                    xc.fetch_add(1, Ordering::SeqCst);
                });
                sm.on_enter(State::Loading, move |_| {
                    ec.fetch_add(1, Ordering::SeqCst);
                });
            })
            .materialize();

        source.set(State::Loading);
        assert_eq!(exit_count.load(Ordering::SeqCst), 1);
        assert_eq!(enter_count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn test_state_transition_guard() {
        let source = Cell::new(State::Idle);
        let allow = Arc::new(AtomicBool::new(false));

        let a = allow.clone();
        let sm = source
            .clone()
            .state_transition(|sm| {
                sm.on(State::Idle, State::Loading, |_, _| true);
                sm.guard(State::Idle, State::Loading, move |_, _| {
                    a.load(Ordering::SeqCst)
                });
            })
            .materialize();

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
        let sm2 = source2
            .clone()
            .state_transition(|sm| {
                sm.on(State::Idle, State::Loading, |_, _| true);
                sm.guard(State::Idle, State::Loading, move |_, _| {
                    a2.load(Ordering::SeqCst)
                });
            })
            .materialize();

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
        let _sm: Cell<bool, _> = source
            .clone()
            .state_transition(|sm| {
                sm.on(State::Idle, State::Loading, |_, _| true);
                sm.on_invalid(move |_, _| {
                    ic.fetch_add(1, Ordering::SeqCst);
                });
            })
            .materialize();

        // Invalid transition
        source.set(State::Ready);
        assert_eq!(invalid_count.load(Ordering::SeqCst), 1);

        // Another invalid
        source.set(State::Error);
        assert_eq!(invalid_count.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn test_state_transition_selective_emit() {
        use crate::{FilterExt, Gettable, Materialize};

        let source = Cell::new(State::Idle);
        let sm = source.clone().state_transition(|sm| {
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
        assert_eq!(triggers.get(), Some(true));
    }
}
