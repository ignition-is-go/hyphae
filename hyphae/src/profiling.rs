//! Counts how often each cell fans out while a pass is open.
//!
//! A *pass* is a stretch of work whose propagation you want to size, such as
//! one store diff or one `EventBatch`. Wrap it in [`pass`]. Until the pass
//! closes, every cell fanout in the process is counted by cell id, on
//! whichever thread settles it. [`take_report`] returns the counts.
//!
//! A cell that holds state fans out once per settle, so a count above one
//! means the pass took more than one settle to reach that cell. Two `set` calls are two
//! settles, and a callback that writes back into an input starts another.
//! Wrap the same writes in [`batch`](crate::batch) and they share a settle:
//! the count drops to one. The report therefore sizes what batching the
//! writes of a pass would save.
//!
//! The count is process-wide. A fanout that another thread's write causes
//! while a pass is open is counted too, and passes open on two threads share
//! one tally and one report. Measure with one pass at a time. A write made
//! with [`submit`](crate::submit) or [`enqueue`](crate::enqueue) can settle
//! after the pass has closed and then goes uncounted.
//!
//! Counting changes no behavior. The module exists only with the `profiling`
//! feature.
//!
//! ```
//! use hyphae::{Cell, MapExt, Materialize, Mutable, Watchable};
//! use hyphae::profiling::{pass, take_report};
//!
//! let a = Cell::new(1);
//! let b = a.clone().map(|x| x + 1).materialize();
//! let guard = b.subscribe(|_| {});
//!
//! pass(|| {
//!     a.set(2);
//!     a.set(3);
//! });
//! let report = take_report().unwrap();
//! assert!(report.total_fires() >= report.total_refires());
//! drop(guard);
//! ```

use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::Mutex;
use rustc_hash::FxHashMap;
use uuid::Uuid;

/// True while a pass is open, so a fanout outside a pass costs one load.
static OPEN: AtomicBool = AtomicBool::new(false);
static PASS: Mutex<PassState> = Mutex::new(PassState {
    depth: 0,
    fires: None,
    last: None,
});

struct PassState {
    /// Passes now open, on any thread. They share the tally.
    depth: u32,
    /// `cell id -> fanouts so far this pass`.
    fires: Option<FxHashMap<Uuid, u32>>,
    /// The report sealed when the last open pass closed, until taken.
    last: Option<PassReport>,
}

/// Per-cell fanout tally for one completed [`pass`].
///
/// `per_cell` holds `(cell id, fanouts in the pass)` for every cell that fired.
/// The derived accessors turn that into the numbers you actually want: how much
/// firing was redundant, and which cells drove it.
#[derive(Debug, Clone, Default)]
pub struct PassReport {
    /// `(cell id, fanouts in the pass)`, one entry per cell that fired at least
    /// once. Unordered.
    pub per_cell: Vec<(Uuid, u32)>,
}

struct PassGuard;

impl Drop for PassGuard {
    fn drop(&mut self) {
        let mut state = PASS.lock();
        state.depth = state.depth.saturating_sub(1);
        if state.depth == 0 {
            OPEN.store(false, Ordering::Release);
            let per_cell = state.fires.take().into_iter().flatten().collect();
            state.last = Some(PassReport { per_cell });
        }
    }
}

impl PassReport {
    /// Total fanouts across all cells in the pass.
    #[must_use]
    pub fn total_fires(&self) -> u64 {
        self.per_cell.iter().map(|&(_, n)| u64::from(n)).sum()
    }

    /// Fanouts beyond the first per cell — the coalesceable re-fires. A cell
    /// that fired once contributes 0; a cell that fired 4× contributes 3.
    #[must_use]
    pub fn total_refires(&self) -> u64 {
        self.per_cell
            .iter()
            .map(|&(_, n)| u64::from(n.saturating_sub(1)))
            .sum()
    }

    /// Number of distinct cells that fired at least once this pass.
    #[must_use]
    pub const fn cells_fired(&self) -> usize {
        self.per_cell.len()
    }

    /// Cells that fired more than once, most re-firing first — the ones that
    /// coalescing collapses. This is where a `4:2:1` harmonic shows up directly.
    #[must_use]
    pub fn refiring_cells(&self) -> Vec<(Uuid, u32)> {
        let mut v: Vec<_> = self
            .per_cell
            .iter()
            .copied()
            .filter(|&(_, n)| n > 1)
            .collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        v
    }

    /// Coalesceable fraction: `total_refires / total_fires`, in `0.0..=1.0`
    /// (`0.0` when nothing fired). The share of this pass's fanouts that a
    /// single settle would eliminate.
    #[must_use]
    pub fn coalesceable_fraction(&self) -> f64 {
        fn u64_to_f64(value: u64) -> f64 {
            let high = u32::try_from(value >> 32).unwrap_or(u32::MAX);
            let low = u32::try_from(value & u64::from(u32::MAX)).unwrap_or(u32::MAX);
            f64::from(high).mul_add(4_294_967_296.0, f64::from(low))
        }

        let total = self.total_fires();
        if total == 0 {
            0.0
        } else {
            u64_to_f64(self.total_refires()) / u64_to_f64(total)
        }
    }
}

/// Run `f` as one measured pass, counting every cell fanout that happens
/// until it returns. Retrieve the counts with [`take_report`].
///
/// Returns `f`'s value untouched. A call made while another pass is open, on
/// this thread or another, joins that pass: the counts start when the first
/// one opens, and the [`PassReport`] is sealed when the last one closes,
/// including on unwind.
pub fn pass<R>(f: impl FnOnce() -> R) -> R {
    let mut state = PASS.lock();
    state.depth = state.depth.saturating_add(1);
    if state.depth == 1 {
        state.fires = Some(FxHashMap::default());
        OPEN.store(true, Ordering::Release);
    }
    drop(state);
    // The guard seals the report and restores depth even if `f` unwinds.
    let _guard = PassGuard;
    f()
}

/// Take the report of the most recently completed pass, if one has been
/// sealed since the last take. The read clears it, so a second call returns
/// `None` until the next [`pass`] completes.
#[must_use]
pub fn take_report() -> Option<PassReport> {
    PASS.lock().last.take()
}

/// Count one fanout of `id` when a pass is open. Called from `Cell::fanout`.
#[inline]
pub(crate) fn record_fire(id: Uuid) {
    if !OPEN.load(Ordering::Acquire) {
        return;
    }
    if let Some(fires) = PASS.lock().fires.as_mut() {
        let count = fires.entry(id).or_insert(0);
        *count = count.saturating_add(1);
    }
}
