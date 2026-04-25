//! `Pipeline` and `PipelineInstall` implementations for source types.
//!
//! Every `Watchable` source — `Cell`, `BoundedInput`, ... — implements
//! `PipelineInstall<T>` via a blanket so chained operators can subscribe
//! to a generic upstream pipeline. `Pipeline<T>` itself is implemented
//! explicitly per source type so that materialize() can be overridden
//! when a no-op is sound.
//!
//! For [`Cell`], `materialize` is a marker flip (same `Arc<inner>`, new
//! `PhantomData<CellImmutable>`) — there is no point allocating a fresh
//! cell + forwarding subscription when the upstream is already a cached,
//! multicast cell. Concrete pipeline structs (`MapPipeline`, ...) provide
//! their own `Pipeline` impls and inherit the default `materialize`.

use std::{marker::PhantomData, sync::Arc};

use crate::{
    bounded_input::BoundedInput,
    cell::{Cell, CellImmutable},
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

#[allow(private_bounds)]
impl<T: CellValue, M: Send + Sync + 'static> Pipeline<T> for Cell<T, M> {
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
impl<T: CellValue> Pipeline<T> for BoundedInput<T> {
    // Inherits the default materialize. BoundedInput has no underlying
    // immutable cell to short-circuit to (the inner cell is wrapped in a
    // bounded-channel structure), so the default allocate-and-forward
    // strategy is correct here.
}
