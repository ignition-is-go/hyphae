//! Calibration-only counters for the join-region left dispatch decision.
//!
//! This module and every call into it are absent from normal builds.

use std::sync::atomic::{AtomicU64, Ordering};

static LEFT_PARALLEL_DISPATCHES: AtomicU64 = AtomicU64::new(0);
static LEFT_SERIAL_DISPATCHES: AtomicU64 = AtomicU64::new(0);
static INACTIVE_TO_PARALLEL: AtomicU64 = AtomicU64::new(0);
static PARALLEL_TO_INACTIVE: AtomicU64 = AtomicU64::new(0);

/// One coherent-enough relaxed snapshot for single-caller calibration runs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub left_parallel_dispatches: u64,
    pub left_serial_dispatches: u64,
    pub inactive_to_parallel: u64,
    pub parallel_to_inactive: u64,
}

impl Snapshot {
    /// Saturating difference, suitable for assertions around one operation.
    #[must_use]
    pub const fn since(self, earlier: Self) -> Self {
        Self {
            left_parallel_dispatches: self
                .left_parallel_dispatches
                .saturating_sub(earlier.left_parallel_dispatches),
            left_serial_dispatches: self
                .left_serial_dispatches
                .saturating_sub(earlier.left_serial_dispatches),
            inactive_to_parallel: self
                .inactive_to_parallel
                .saturating_sub(earlier.inactive_to_parallel),
            parallel_to_inactive: self
                .parallel_to_inactive
                .saturating_sub(earlier.parallel_to_inactive),
        }
    }
}

/// Read all calibration counters.
#[must_use]
pub fn snapshot() -> Snapshot {
    Snapshot {
        left_parallel_dispatches: LEFT_PARALLEL_DISPATCHES.load(Ordering::Relaxed),
        left_serial_dispatches: LEFT_SERIAL_DISPATCHES.load(Ordering::Relaxed),
        inactive_to_parallel: INACTIVE_TO_PARALLEL.load(Ordering::Relaxed),
        parallel_to_inactive: PARALLEL_TO_INACTIVE.load(Ordering::Relaxed),
    }
}

pub(crate) fn left_parallel_dispatch() {
    LEFT_PARALLEL_DISPATCHES.fetch_add(1, Ordering::Relaxed);
}
pub(crate) fn left_serial_dispatch() {
    LEFT_SERIAL_DISPATCHES.fetch_add(1, Ordering::Relaxed);
}
pub(crate) fn inactive_to_parallel() {
    INACTIVE_TO_PARALLEL.fetch_add(1, Ordering::Relaxed);
}
pub(crate) fn parallel_to_inactive() {
    PARALLEL_TO_INACTIVE.fetch_add(1, Ordering::Relaxed);
}
