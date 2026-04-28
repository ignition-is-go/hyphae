//! `Pipeline` and supporting trait implementations for source types.
//!
//! Every `Watchable` source — `Cell`, `BoundedInput`, ... — implements
//! [`PipelineInstall`] via a blanket so chained operators can subscribe
//! to a generic upstream pipeline. [`PipelineSeed`] is implemented for
//! sources that have a definite current value.
//!
//! [`Pipeline<T, Definite>`] is implemented explicitly per source type.
//! [`MaterializeDefinite`] is overridden on [`Cell`] to skip the cell+forward
//! allocation: a cell is already a cached, multicast source, so materializing
//! is just a marker flip on the same `Arc<inner>`.

use std::{marker::PhantomData, sync::Arc};

use crate::{
    bounded_input::BoundedInput,
    cell::{Cell, CellImmutable},
    pipeline::{Definite, MaterializeDefinite, Pipeline, PipelineInstall, PipelineSeed},
    signal::Signal,
    subscription::SubscriptionGuard,
    traits::{CellValue, Gettable, Watchable},
};

impl<T: CellValue, W: Watchable<T>> PipelineInstall<T> for W {
    fn install(
        &self,
        callback: Arc<dyn Fn(&Signal<T>) + Send + Sync>,
    ) -> SubscriptionGuard {
        self.subscribe(move |signal| callback(signal))
    }
}

impl<T: CellValue, M: Send + Sync + 'static> PipelineSeed<T> for Cell<T, M> {
    fn seed(&self) -> T {
        self.get()
    }
}

impl<T: CellValue> PipelineSeed<T> for BoundedInput<T> {
    fn seed(&self) -> T {
        self.get()
    }
}

#[allow(private_bounds)]
impl<T: CellValue, M: Send + Sync + 'static> Pipeline<T, Definite> for Cell<T, M> {}

impl<T: CellValue, M: Send + Sync + 'static> MaterializeDefinite<T> for Cell<T, M> {
    /// No-op materialize: the cell is already a cached, multicast source.
    /// Just flip the marker to `CellImmutable` and reuse the same `Arc<inner>`.
    fn materialize(self) -> Cell<T, CellImmutable> {
        Cell {
            inner: self.inner,
            _marker: PhantomData,
        }
    }
}

#[allow(private_bounds)]
impl<T: CellValue> Pipeline<T, Definite> for BoundedInput<T> {}

impl<T: CellValue> MaterializeDefinite<T> for BoundedInput<T> {
    // Default body is correct: BoundedInput has no underlying immutable cell
    // to short-circuit to, so allocate a fresh cell + forwarding subscription.
}
