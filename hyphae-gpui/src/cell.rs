use std::sync::{Arc, Mutex};

use gpui::{App, AppContext, Context, Entity, Subscription, Task};
use hyphae::{Cell, CellValue, Signal, SubscriptionGuard, Watchable};

/// Terminal state received from the Hyphae cell.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CellEntityStatus {
    /// The cell may still emit values.
    Active,
    /// The cell completed normally.
    Complete,
    /// The cell terminated with an error.
    Error(String),
}

enum CellEvent<T> {
    Value(T),
    Complete,
    Error(String),
}

struct Bootstrap<T> {
    initializing: bool,
    value: Option<T>,
    status: CellEntityStatus,
}

impl<T> Bootstrap<T> {
    const fn new() -> Self {
        Self {
            initializing: true,
            value: None,
            status: CellEntityStatus::Active,
        }
    }

    fn apply(&mut self, event: CellEvent<T>) {
        match event {
            CellEvent::Value(value) => self.value = Some(value),
            CellEvent::Complete => self.status = CellEntityStatus::Complete,
            CellEvent::Error(error) => self.status = CellEntityStatus::Error(error),
        }
    }
}

/// GPUI-owned snapshot of a Hyphae cell.
///
/// The entity owns both the Hyphae subscription and the foreground driver, so
/// dropping it stops both sides of the bridge.
pub struct CellEntity<T: CellValue> {
    value: Option<T>,
    status: CellEntityStatus,
    _subscription: SubscriptionGuard,
    _driver: Task<()>,
}

impl<T: CellValue> CellEntity<T> {
    /// Latest value delivered by the subscription.
    #[must_use]
    pub const fn value(&self) -> Option<&T> {
        self.value.as_ref()
    }

    /// Whether the upstream cell is active, complete, or errored.
    #[must_use]
    pub const fn status(&self) -> &CellEntityStatus {
        &self.status
    }

    fn apply(&mut self, event: CellEvent<T>) {
        match event {
            CellEvent::Value(value) => self.value = Some(value),
            CellEvent::Complete => self.status = CellEntityStatus::Complete,
            CellEvent::Error(error) => self.status = CellEntityStatus::Error(error),
        }
    }
}

/// Convert any mutable or immutable Hyphae cell into a GPUI entity.
pub trait ToGpuiEntity<T: CellValue> {
    /// Subscribe to this cell and create an entity driven only by notifications.
    fn to_gpui_entity(&self, cx: &mut App) -> Entity<CellEntity<T>>;
}

impl<T, M> ToGpuiEntity<T> for Cell<T, M>
where
    T: CellValue,
    M: Send + Sync + 'static,
{
    fn to_gpui_entity(&self, cx: &mut App) -> Entity<CellEntity<T>> {
        let (sender, receiver) = flume::unbounded();
        let bootstrap = Arc::new(Mutex::new(Bootstrap::new()));
        let callback_bootstrap = Arc::clone(&bootstrap);
        let subscription = self.subscribe(move |signal| {
            let event = match signal {
                Signal::Value(value) => CellEvent::Value((**value).clone()),
                Signal::Complete => CellEvent::Complete,
                Signal::Error(error) => CellEvent::Error(error.to_string()),
            };

            // Hyphae guarantees its seed replay and any concurrently queued
            // notifications finish before `subscribe` returns. Keep the last
            // replayed value locally so construction needs no separate `get`.
            let mut state = callback_bootstrap
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.initializing {
                state.apply(event);
            } else {
                drop(state);
                let _ = sender.send(event);
            }
        });

        let (initial, status) = {
            let mut state = bootstrap
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.initializing = false;
            (state.value.take(), state.status.clone())
        };

        cx.new(move |cx| {
            let driver = cx.spawn(async move |entity, cx| {
                loop {
                    // Await the channel on GPUI's background executor. On Wasm,
                    // completing that task re-enters through GPUI's platform
                    // scheduler, avoiding the shared-memory channel waker gap
                    // seen when `recv_async` is polled directly by the local
                    // foreground executor. There is no timer or frame polling.
                    let receive = receiver.clone();
                    let event = cx
                        .background_executor()
                        .spawn(async move { receive.recv_async().await })
                        .await;
                    let Ok(event) = event else {
                        break;
                    };
                    if entity
                        .update(cx, |state: &mut CellEntity<T>, cx| {
                            state.apply(event);
                            cx.notify();
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            });
            CellEntity {
                value: initial,
                status,
                _subscription: subscription,
                _driver: driver,
            }
        })
    }
}

/// Typed shorthand for observing a [`CellEntity`] from another GPUI entity.
pub trait ObserveCellEntityExt<Owner: 'static> {
    /// Run `on_change` whenever the bridged cell entity is notified.
    fn observe_cell<T: CellValue>(
        &mut self,
        entity: &Entity<CellEntity<T>>,
        on_change: impl FnMut(&mut Owner, &T, &mut Context<Owner>) + 'static,
    ) -> Subscription;
}

impl<Owner: 'static> ObserveCellEntityExt<Owner> for Context<'_, Owner> {
    fn observe_cell<T: CellValue>(
        &mut self,
        entity: &Entity<CellEntity<T>>,
        mut on_change: impl FnMut(&mut Owner, &T, &mut Context<Owner>) + 'static,
    ) -> Subscription {
        self.observe(entity, move |owner, entity, cx| {
            let value = entity.read_with(cx, |cell, _| cell.value().cloned());
            if let Some(value) = value {
                on_change(owner, &value, cx);
            }
        })
    }
}

#[cfg(test)]
#[allow(clippy::needless_pass_by_ref_mut)]
mod tests {
    use gpui::{AppContext as _, TestAppContext};
    use hyphae::{Cell, Mutable};

    use super::{CellEntityStatus, ObserveCellEntityExt as _, ToGpuiEntity};

    struct Observer {
        value: u32,
        _observation: gpui::Subscription,
    }

    #[gpui::test]
    fn observation_helper_delivers_values(cx: &mut TestAppContext) {
        let cell = Cell::new(3_u32);
        let bridged = cx.update(|cx| cell.to_gpui_entity(cx));
        let observer = cx.update(|cx| {
            cx.new(|cx| {
                let observation = cx.observe_cell(&bridged, |observer: &mut Observer, value, _| {
                    observer.value = *value;
                });
                Observer {
                    value: 0,
                    _observation: observation,
                }
            })
        });
        cx.run_until_parked();
        assert_eq!(observer.read_with(cx, |observer, _| observer.value), 0);

        cell.set(9);
        cx.run_until_parked();
        assert_eq!(observer.read_with(cx, |observer, _| observer.value), 9);
    }

    #[gpui::test]
    fn synchronous_seed_initializes_without_waiting_for_the_driver(cx: &mut TestAppContext) {
        let cell = Cell::new(1_u32);
        let entity = cx.update(|cx| cell.to_gpui_entity(cx));
        assert_eq!(
            entity.read_with(cx, |entity, _| entity.value().copied()),
            Some(1)
        );

        cell.set(7);
        cx.run_until_parked();
        assert_eq!(
            entity.read_with(cx, |entity, _| entity.value().copied()),
            Some(7)
        );
        assert_eq!(
            entity.read_with(cx, |entity, _| entity.status().clone()),
            CellEntityStatus::Active
        );
    }
}
