//! Within-pass re-fire measurement — sizing how much of a propagation is
//! redundant re-fire, and thus coalesceable, **without changing any behavior**.
//!
//! A *pass* is one logical propagation boundary. For the motivating case
//! (rship's server-side fanout) that is one store-diff / `EventBatch`
//! application: wrap it in [`pass`], and while that scope is on the stack every
//! cell **fanout** — one per emit — is tallied by cell id. A cell that fans out
//! more than once in a single pass is *re-firing*: in a glitch-free coalesced
//! world it would emit once with its settled value, so every fire past the
//! first is coalesceable.
//!
//! This is pure measurement on the synchronous path — it defers, coalesces, and
//! reorders nothing. Two ways to use it:
//!
//! - On today's synchronous build, [`pass`] reports the full re-fire count —
//!   direct proof of the coalesceable fraction before you adopt [`batch`].
//! - Run the *same* pass with [`batch`] active and each cell's count collapses
//!   toward 1: that is the coalescing landing, measured the same way, so the
//!   two numbers are directly comparable (an A/B on one metric).
//!
//! Gated behind `profiling`; compiles to nothing otherwise.
//!
//! ```
//! use hyphae::{Cell, MapExt, MaterializeDefinite, Mutable, Watchable};
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

use std::cell::RefCell;

use rustc_hash::FxHashMap;
use uuid::Uuid;

thread_local! {
    static PASS: RefCell<PassState> = RefCell::new(PassState::new());
}

/// Per-thread pass state: the reentrancy depth, the in-flight per-cell tally,
/// and the most recently sealed report.
struct PassState {
    /// Reentrancy depth. `> 0` means a pass is active and fanouts are tallied;
    /// nested [`pass`] calls join the outermost pass.
    depth: u32,
    /// `cell id -> fanouts so far this pass`.
    fires: FxHashMap<Uuid, u32>,
    /// The report sealed at the last outermost pass exit, until taken.
    last: Option<PassReport>,
}

impl PassState {
    fn new() -> Self {
        Self {
            depth: 0,
            fires: FxHashMap::default(),
            last: None,
        }
    }
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

impl PassReport {
    /// Total fanouts across all cells in the pass.
    pub fn total_fires(&self) -> u64 {
        self.per_cell.iter().map(|&(_, n)| n as u64).sum()
    }

    /// Fanouts beyond the first per cell — the coalesceable re-fires. A cell
    /// that fired once contributes 0; a cell that fired 4× contributes 3.
    pub fn total_refires(&self) -> u64 {
        self.per_cell
            .iter()
            .map(|&(_, n)| n.saturating_sub(1) as u64)
            .sum()
    }

    /// Number of distinct cells that fired at least once this pass.
    pub fn cells_fired(&self) -> usize {
        self.per_cell.len()
    }

    /// Cells that fired more than once, most re-firing first — the ones that
    /// coalescing collapses. This is where a `4:2:1` harmonic shows up directly.
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
    /// glitch-free coalesced drain would eliminate.
    pub fn coalesceable_fraction(&self) -> f64 {
        let total = self.total_fires();
        if total == 0 {
            0.0
        } else {
            self.total_refires() as f64 / total as f64
        }
    }
}

/// Run `f` as one measured pass, tallying every cell fanout that happens inside
/// it. Retrieve the tally with [`take_report`] after `f` returns.
///
/// Returns `f`'s value untouched. Nested calls join the outermost pass — only
/// the outermost resets the tally on entry and seals the [`PassReport`] on exit
/// (including on unwind), so a panic inside `f` still leaves a clean slate.
///
/// This measures whatever fanouts occur, so it composes with [`batch`]: wrap
/// `pass(|| batch(|| ..))` (or the reverse) and the report reflects the
/// coalesced fanout count.
pub fn pass<R>(f: impl FnOnce() -> R) -> R {
    PASS.with(|p| {
        let mut p = p.borrow_mut();
        p.depth += 1;
        if p.depth == 1 {
            p.fires.clear();
        }
    });
    // Guard seals the report and restores depth even if `f` unwinds.
    struct Guard;
    impl Drop for Guard {
        fn drop(&mut self) {
            PASS.with(|p| {
                let mut p = p.borrow_mut();
                p.depth -= 1;
                if p.depth == 0 {
                    let per_cell = p.fires.drain().collect();
                    p.last = Some(PassReport { per_cell });
                }
            });
        }
    }
    let _guard = Guard;
    f()
}

/// Take the most recently completed pass's report, if one has been sealed since
/// the last take. The read clears it, so a second call returns `None` until the
/// next [`pass`] completes.
pub fn take_report() -> Option<PassReport> {
    PASS.with(|p| p.borrow_mut().last.take())
}

/// Tally one fanout for `id` when a pass is active. Called from `Cell::fanout`;
/// a no-op (one thread-local borrow and a depth check) outside a pass.
#[inline]
pub(crate) fn record_fire(id: Uuid) {
    PASS.with(|p| {
        let mut p = p.borrow_mut();
        if p.depth > 0 {
            *p.fires.entry(id).or_insert(0) += 1;
        }
    });
}
