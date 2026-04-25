//! Blanket `Pipeline` implementation for any [`Watchable`].
//!
//! Every `Watchable` source — `Cell`, `BoundedInput`, etc. — implements
//! `Pipeline<T>` via this blanket, so `cell.map(...)`, `bounded.filter(...)`,
//! and other pure-operator entry points compile without per-type wiring.
//! Concrete pipeline structs (`MapPipeline`, ...) are NOT `Watchable` and
//! provide their own `Pipeline` impls.

use std::sync::Arc;

use crate::{
    pipeline::{Pipeline, PipelineInstall},
    signal::Signal,
    subscription::SubscriptionGuard,
    traits::{CellValue, Watchable},
};

impl<T: CellValue, W: Watchable<T>> PipelineInstall<T> for W {
    fn install(
        &self,
        callback: Arc<dyn Fn(&Signal<T>) + Send + Sync>,
    ) -> SubscriptionGuard {
        self.subscribe(move |signal| callback(signal))
    }
}

impl<T: CellValue, W: Watchable<T>> Pipeline<T> for W {}
