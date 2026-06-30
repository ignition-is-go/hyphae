//! Platform abstraction over APIs that differ between native and wasm targets.
//!
//! The reactive core of hyphae is platform-agnostic, but a handful of
//! primitives — monotonic time, deferred/repeating timers, and parallel
//! subscriber fan-out — have no portable std implementation on
//! `wasm32-unknown-unknown` (no threads, no `Instant::now`, no `spin_sleep`).
//!
//! This module exposes a single API with two implementations:
//!
//! - **native** ([`native`]): threads + [`std::time::Instant`] + `spin_sleep`
//!   + `rayon`.
//! - **wasm** ([`wasm`]): `setTimeout`/`setInterval` via `gloo-timers`,
//!   `performance.now()` via `web-time`, and a sequential fan-out fallback.
//!
//! Keeping the surface identical across targets is what lets the timer-based
//! operators (`delay`, `debounce`, `throttle`, `timeout`, `audit`,
//! `buffer_time`, the `interval` constructors) and `ParallelExt` compile and
//! *run* in the browser — a prerequisite for bridging hyphae cells to
//! front-end signal libraries such as Leptos.

#[cfg(not(target_arch = "wasm32"))]
mod native;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use native::{Instant, par_for_each, spawn_delayed, spawn_interval};

#[cfg(target_arch = "wasm32")]
mod wasm;
#[cfg(target_arch = "wasm32")]
pub(crate) use wasm::{Instant, par_for_each, spawn_delayed, spawn_interval};
