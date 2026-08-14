//! Event-driven adapters from [Hyphae](hyphae) reactivity to [GPUI](gpui).
//!
//! A bridge subscribes once and forwards each Hyphae notification onto GPUI's
//! foreground executor. It never polls during rendering or on a frame timer.
//! Cell construction is seeded directly by Hyphae's synchronous subscription
//! replay rather than a separate read. Calling `cx.notify()` is therefore
//! causally tied to an upstream notification.
//!
//! ```ignore
//! use hyphae::{Cell, Mutable};
//! use hyphae_gpui::{ObserveCellEntityExt, ToGpuiEntity};
//!
//! let temperature = Cell::new(20);
//! let temperature = temperature.to_gpui_entity(cx);
//! let _observation = cx.observe_cell(&temperature, |view, value, cx| {
//!     view.temperature = *value;
//!     cx.notify();
//! });
//! ```
//!
//! [`CellMapEntity`] is fine-grained: membership changes notify the collection
//! entity, while a value update notifies only that key's [`MapEntry`] entity.

mod cell;
mod map;

pub use cell::{CellEntity, CellEntityStatus, ObserveCellEntityExt, ToGpuiEntity};
pub use map::{CellMapEntity, MapEntry, ToGpuiMapEntity};
