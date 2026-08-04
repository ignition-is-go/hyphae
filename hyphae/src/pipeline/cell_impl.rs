//! `Pipeline` and supporting trait implementations for source types.
//!
//! Every `Watchable` source — `Cell`, `BoundedInput`, ... — implements
//! [`PipelineInstall`] via a blanket so chained operators can subscribe
//! to a generic upstream pipeline. [`PipelineSeed`] is implemented for
//! sources that have a definite current value.
//!
//! [`Pipeline<T, Definite>`] is implemented explicitly per source type.
//! [`Materialize`] is overridden on [`Cell`] to skip the cell+forward
//! allocation: a cell is already a cached, multicast source, so materializing
//! is just a marker flip on the same `Arc<inner>`.

use std::{marker::PhantomData, sync::Arc};

use crate::{
    bounded_input::BoundedInput,
    cell::{Cell, CellImmutable},
    pipeline::{Definite, Pipeline, PipelineInstall, PipelineSeed},
    signal::Signal,
    subscription::SubscriptionGuard,
    traits::{CellValue, Gettable, Watchable},
};

// Per-source `PipelineInstall` impls. We don't blanket-impl over `W: Watchable`
// because that would prevent us from impl-ing `PipelineInstall` for non-
// `Watchable` types like `Source<T>` (which is intentionally not `Gettable`).
impl<T: CellValue, M: Send + Sync + 'static> PipelineInstall<T> for Cell<T, M> {
    fn install(&self, callback: Arc<dyn Fn(&Signal<T>) + Send + Sync>) -> SubscriptionGuard {
        self.subscribe(move |signal| callback(signal))
    }
}

impl<T: CellValue> PipelineInstall<T> for BoundedInput<T> {
    fn install(&self, callback: Arc<dyn Fn(&Signal<T>) + Send + Sync>) -> SubscriptionGuard {
        self.subscribe(move |signal| callback(signal))
    }
}

impl<T: CellValue, M: Send + Sync + 'static> PipelineSeed<T> for Cell<T, M> {
    fn seed(&self) -> T {
        self.get()
    }

    fn materialize_definite(self) -> Cell<T, CellImmutable> {
        Cell {
            inner: self.inner,
            _marker: PhantomData,
        }
    }
}

impl<T: CellValue> PipelineSeed<T> for BoundedInput<T> {
    fn seed(&self) -> T {
        self.get()
    }
}

#[allow(private_bounds)]
impl<T: CellValue, M: Send + Sync + 'static> Pipeline<T, Definite> for Cell<T, M> {}
