//! Blanket `Pipeline` implementation for `Cell`.

use std::sync::Arc;

use crate::{
    cell::Cell,
    pipeline::{Pipeline, PipelineInstall},
    signal::Signal,
    subscription::SubscriptionGuard,
    traits::{CellValue, Watchable},
};

impl<T: CellValue, M: Send + Sync + 'static> PipelineInstall<T> for Cell<T, M> {
    fn install(
        &self,
        callback: Arc<dyn Fn(&Signal<T>) + Send + Sync>,
    ) -> SubscriptionGuard {
        // Delegate to the cell's native subscribe — the callback receives signals
        // untransformed because a plain Cell's pipeline op is identity.
        self.subscribe(move |signal| callback(signal))
    }
}

impl<T: CellValue, M: Send + Sync + 'static> Pipeline<T> for Cell<T, M> {}
