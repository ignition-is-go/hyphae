//! Opt-in propagation scheduler — deferred, ordered, glitch-free flushes under
//! an explicit [`batch`] boundary, driven by a pluggable [`crate::clock`].
//!
//! Gated behind the `scheduler` feature. When the feature is off, or when code
//! runs outside a [`batch`], propagation takes hyphae's exact synchronous
//! eager-push path — this module changes nothing about the default.
//!
//! **Phase 0 groundwork.** The clock seam and the observability substrate land
//! first so we can measure at every step. [`batch`] currently runs its closure
//! inline (no deferral); the thread-local tick context, the height-ordered
//! coalescing drain, and the single `Cell::notify` interception are added in a
//! later phase. The public entry point exists now so downstream code and
//! benchmarks compile against a stable API.

/// Run `f` as a single propagation batch.
///
/// Once the deferred drain lands, every `set` inside `f` will enqueue instead of
/// cascading, and the whole downstream DAG will flush once, in height order,
/// glitch-free, at the closing brace. Today `f` runs inline (identical to
/// calling the closures directly), so this is a behavior-preserving placeholder.
pub fn batch<R>(f: impl FnOnce() -> R) -> R {
    f()
}
