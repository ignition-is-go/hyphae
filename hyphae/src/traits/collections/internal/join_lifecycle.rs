use std::{
    hash::Hash,
    marker::PhantomData,
    sync::{Arc, Mutex},
};

use crate::{
    cell_map::MapDiff,
    map_query::{
        BoxedMapDiffSink, MapQuery, compile_runtime_into,
        compiler::{CompileContext, QUERY_POISONED_MESSAGE, QueryPoison},
    },
    subscription::SubscriptionGuard,
    traits::CellValue,
};

pub(super) enum RuntimeStorage<Serial, Sharded> {
    Serial(Serial),
    Sharded {
        runtime: Sharded,
        parallel_active: bool,
    },
}

#[allow(clippy::panic)]
impl<Serial, Sharded> RuntimeStorage<Serial, Sharded> {
    pub(super) const fn is_serial(&self) -> bool {
        matches!(self, Self::Serial(_))
    }

    pub(super) fn serial_mut(&mut self) -> &mut Serial {
        match self {
            Self::Serial(runtime) => runtime,
            Self::Sharded { .. } => {
                std::panic::panic_any("join runtime invariant violated: expected serial storage")
            }
        }
    }

    pub(super) fn sharded_mut(&mut self) -> (&mut Sharded, &mut bool) {
        match self {
            Self::Sharded {
                runtime,
                parallel_active,
            } => (runtime, parallel_active),
            Self::Serial(_) => {
                std::panic::panic_any("join runtime invariant violated: expected sharded storage")
            }
        }
    }

    pub(super) fn promote_with(&mut self, build: impl FnOnce(&Serial) -> Sharded) -> bool {
        let Self::Serial(serial) = self else {
            return false;
        };
        let runtime = build(serial);
        *self = Self::Sharded {
            runtime,
            parallel_active: false,
        };
        true
    }
}

pub(super) trait PublishChanges<K, Output> {
    fn publish(self, sink: &BoxedMapDiffSink<K, Output>);
}

impl<K, Output> PublishChanges<K, Output> for Vec<MapDiff<K, Output>> {
    fn publish(self, sink: &BoxedMapDiffSink<K, Output>) {
        for change in &self {
            sink(change);
        }
    }
}

pub(super) struct BatchedChanges<K, Output>(pub(super) Vec<MapDiff<K, Output>>);

impl<K, Output> PublishChanges<K, Output> for BatchedChanges<K, Output> {
    fn publish(self, sink: &BoxedMapDiffSink<K, Output>) {
        if !self.0.is_empty() {
            sink(&MapDiff::Batch { changes: self.0 });
        }
    }
}

pub(super) trait TransactionPolicy<State>: Send + Sync + 'static {
    type Stored: Send;

    fn wrap(self, state: State) -> Self::Stored;

    fn run<T>(stored: &Mutex<Self::Stored>, apply: impl FnOnce(&mut State) -> T) -> T;
}

pub(super) struct LegacyTransaction;

impl<State: Send> TransactionPolicy<State> for LegacyTransaction {
    type Stored = State;

    fn wrap(self, state: State) -> Self::Stored {
        state
    }

    fn run<T>(stored: &Mutex<Self::Stored>, apply: impl FnOnce(&mut State) -> T) -> T {
        let mut state = stored
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        apply(&mut state)
    }
}

pub(super) struct FailStopTransaction {
    query_poison: QueryPoison,
}

impl FailStopTransaction {
    pub(super) const fn new(query_poison: QueryPoison) -> Self {
        Self { query_poison }
    }
}

pub(super) struct FailStopState<State> {
    kernel: State,
    poisoned: bool,
    query_poison: QueryPoison,
}

impl<State: Send> TransactionPolicy<State> for FailStopTransaction {
    type Stored = FailStopState<State>;

    fn wrap(self, state: State) -> Self::Stored {
        FailStopState {
            kernel: state,
            poisoned: false,
            query_poison: self.query_poison,
        }
    }

    #[allow(clippy::panic)]
    fn run<T>(stored: &Mutex<Self::Stored>, apply: impl FnOnce(&mut State) -> T) -> T {
        let mut state = stored
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.poisoned {
            drop(state);
            std::panic::panic_any(QUERY_POISONED_MESSAGE);
        }

        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| apply(&mut state.kernel))) {
            Ok(output) => output,
            Err(payload) => {
                state.query_poison.poison();
                state.poisoned = true;
                drop(state);
                std::panic::resume_unwind(payload);
            }
        }
    }
}

pub(super) struct RegionHost<State, K, Output, Policy>
where
    Policy: TransactionPolicy<State>,
{
    state: Mutex<Policy::Stored>,
    sink: BoxedMapDiffSink<K, Output>,
    _policy: PhantomData<fn() -> (State, Policy)>,
}

impl<State, K, Output, Policy> RegionHost<State, K, Output, Policy>
where
    K: Hash + Eq + CellValue,
    Output: CellValue,
    Policy: TransactionPolicy<State>,
{
    fn new(state: State, policy: Policy, sink: BoxedMapDiffSink<K, Output>) -> Self {
        Self {
            state: Mutex::new(policy.wrap(state)),
            sink,
            _policy: PhantomData,
        }
    }

    #[inline]
    pub(super) fn dispatch<Changes>(&self, apply: impl FnOnce(&mut State) -> Changes)
    where
        Changes: PublishChanges<K, Output>,
    {
        let changes = Policy::run(&self.state, apply);
        changes.publish(&self.sink);
    }
}

#[derive(Clone, Copy)]
pub(super) enum RootRegistrationOrder {
    LeftThenRights,
    RightsThenLeft,
}

pub(super) trait InstallRegionRights<State, K, Output, Policy>
where
    K: Hash + Eq + CellValue,
    Output: CellValue,
    Policy: TransactionPolicy<State>,
{
    fn install(
        self,
        cx: &mut CompileContext,
        host: &Arc<RegionHost<State, K, Output, Policy>>,
    ) -> Vec<SubscriptionGuard>;
}

fn install_left<Left, State, K, Input, Output, Changes, Apply, Policy>(
    left: Left,
    cx: &mut CompileContext,
    host: &Arc<RegionHost<State, K, Output, Policy>>,
    apply_left: Apply,
) -> Vec<SubscriptionGuard>
where
    K: Hash + Eq + CellValue,
    Input: CellValue,
    Output: CellValue,
    Left: MapQuery<Key = K, Value = Input>,
    State: 'static,
    Changes: PublishChanges<K, Output>,
    Apply: Fn(&mut State, &MapDiff<K, Input>) -> Changes + Send + Sync + 'static,
    Policy: TransactionPolicy<State>,
{
    let left_host = Arc::clone(host);
    compile_runtime_into(
        left,
        cx,
        Arc::new(move |diff: &MapDiff<K, Input>| {
            left_host.dispatch(|kernel| apply_left(kernel, diff));
        }),
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn install_region_runtime<
    Left,
    Rights,
    State,
    K,
    Input,
    Output,
    Changes,
    Apply,
    Policy,
>(
    cx: &mut CompileContext,
    left: Left,
    rights: Rights,
    state: State,
    root_order: RootRegistrationOrder,
    policy: Policy,
    sink: BoxedMapDiffSink<K, Output>,
    apply_left: Apply,
) -> Vec<SubscriptionGuard>
where
    K: Hash + Eq + CellValue,
    Input: CellValue,
    Output: CellValue,
    Left: MapQuery<Key = K, Value = Input>,
    State: 'static,
    Changes: PublishChanges<K, Output>,
    Apply: Fn(&mut State, &MapDiff<K, Input>) -> Changes + Send + Sync + 'static,
    Policy: TransactionPolicy<State>,
    Rights: InstallRegionRights<State, K, Output, Policy>,
{
    let host = Arc::new(RegionHost::new(state, policy, sink));
    match root_order {
        RootRegistrationOrder::LeftThenRights => {
            let mut guards = install_left(left, cx, &host, apply_left);
            guards.extend(rights.install(cx, &host));
            guards
        }
        RootRegistrationOrder::RightsThenLeft => {
            let mut guards = rights.install(cx, &host);
            guards.extend(install_left(left, cx, &host, apply_left));
            guards
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use crate::map_query::compiler::{QUERY_POISONED_MESSAGE, QueryPoison};

    use super::{FailStopTransaction, LegacyTransaction, RuntimeStorage, TransactionPolicy};

    #[test]
    fn runtime_storage_promotes_once() {
        let mut storage = RuntimeStorage::Serial(3_u8);

        assert_eq!(*storage.serial_mut(), 3);
        assert!(storage.promote_with(|serial| usize::from(*serial) + 1));
        let (runtime, parallel_active) = storage.sharded_mut();
        assert_eq!(*runtime, 4);
        assert!(!*parallel_active);
        assert!(!storage.promote_with(|_| 99));
        assert_eq!(*storage.sharded_mut().0, 4);
    }

    #[test]
    #[allow(clippy::panic)]
    fn failed_promotion_keeps_serial_runtime() {
        let mut storage = RuntimeStorage::<_, usize>::Serial(7_u8);

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            storage.promote_with(|_| std::panic::panic_any("promotion failed"));
        }));

        assert!(result.is_err());
        assert!(storage.is_serial());
        assert_eq!(*storage.serial_mut(), 7);
    }

    #[test]
    #[allow(clippy::panic)]
    fn legacy_transaction_recovers_the_mutated_state_after_panic() {
        let state = Mutex::new(1_usize);
        let first = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            <LegacyTransaction as TransactionPolicy<usize>>::run(&state, |value| {
                *value = 2;
                std::panic::panic_any("legacy callback failed");
            });
        }));

        assert!(first.is_err());
        let value = <LegacyTransaction as TransactionPolicy<usize>>::run(&state, |value| {
            *value += 1;
            *value
        });
        assert_eq!(value, 3);
    }

    #[test]
    #[allow(clippy::panic)]
    fn fail_stop_transaction_rejects_every_callback_after_panic() {
        let state = Mutex::new(FailStopTransaction::new(QueryPoison::default()).wrap(1_usize));
        let first = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            <FailStopTransaction as TransactionPolicy<usize>>::run(&state, |value| {
                *value = 2;
                std::panic::panic_any("region callback failed");
            });
        }));
        assert!(first.is_err());

        let second = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            <FailStopTransaction as TransactionPolicy<usize>>::run(&state, |value| {
                *value += 1;
            });
        }));
        let payload = second.expect_err("poisoned region must reject later callbacks");
        assert_eq!(
            payload.downcast_ref::<&str>(),
            Some(&QUERY_POISONED_MESSAGE)
        );
    }
}
