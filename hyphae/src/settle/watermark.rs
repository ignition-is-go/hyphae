//! How a node learns that no input will deliver a ticket at or below `t`
//! again: the pull-walk of `docs/settle-epochs.md` section 4.
//!
//! The scheduler asks only [`in_w_past`]. On failure it parks on the blocker
//! returned and re-checks (`evaluate` in `settle.rs`), and a blocker wakes
//! its parked nodes when it fires. Section 4 is being redesigned; a new
//! mechanism replaces this module and keeps that contract.

use std::sync::{Arc, Weak, atomic::Ordering};

use rustc_hash::FxHashSet;

use super::{CYCLE_SITES, CYCLES, INVALIDATED, NodeCore, QUIET_HITS, SEEDED, VALIDATED, WALK_NODES, WALKS};

pub(super) const NOT_QUIET: u8 = 0;
const TENTATIVE: u8 = 1;
const QUIET: u8 = 2;

fn inputs(node: &NodeCore) -> Vec<Arc<NodeCore>> {
    node.inputs
        .lock()
        .iter()
        .filter_map(Weak::upgrade)
        .collect()
}

fn quiet(node: &NodeCore) -> bool {
    node.mark.load(Ordering::Acquire) == QUIET
}

/// True when every input of `core` has promised past `ticket`: `in_w(n) >
/// ticket`. Otherwise the node found holding work at or below `ticket`, for
/// the caller to park on.
///
/// A quiet input counts as `S`, which is past every seeded ticket. An input
/// that is not quiet is certified if it can be; otherwise its watermark is
/// pulled from its ancestry.
pub(super) fn in_w_past(core: &Arc<NodeCore>, ticket: u64) -> Result<(), Arc<NodeCore>> {
    for input in inputs(core) {
        input_past(core, &input, ticket)?;
    }
    Ok(())
}

/// True when `input` of `core` has promised past `ticket`. Otherwise the node
/// found holding work at or below `ticket`.
pub(super) fn input_past(
    core: &Arc<NodeCore>,
    input: &Arc<NodeCore>,
    ticket: u64,
) -> Result<(), Arc<NodeCore>> {
    if quiet(input) {
        QUIET_HITS.fetch_add(1, Ordering::Relaxed);
        return Ok(());
    }
    if input.watermark.load(Ordering::Acquire) > ticket {
        return Ok(());
    }
    let mut certified = 0;
    let valid = validate(input, &mut FxHashSet::default(), &mut certified);
    if certified > 0 && diag::sampled() {
        diag::validation(core.caller.get().copied(), input.caller.get().copied(), certified);
    }
    if valid {
        return Ok(());
    }
    WALKS.fetch_add(1, Ordering::Relaxed);
    let mut seen = FxHashSet::default();
    seen.insert(Arc::as_ptr(core).addr());
    let mut cost = 0;
    let outcome = passed(input, core, ticket, &mut seen, &mut cost);
    WALK_NODES.fetch_add(cost, Ordering::Relaxed);
    outcome
}

/// The bound `in_w` gives a node's watermark when it fires: `S` for a quiet
/// input, its watermark otherwise. O(inputs).
pub(super) fn refresh_bound(core: &NodeCore) -> u64 {
    let mut low = SEEDED.load(Ordering::Acquire);
    for input in inputs(core) {
        if !quiet(&input) {
            low = low.min(input.watermark.load(Ordering::Acquire));
        }
    }
    low
}

/// Clear the certificates of `node` and of every certified descendant. A
/// node that is not certified has no certified descendant, so the walk stops
/// there.
pub(super) fn invalidate(node: &Arc<NodeCore>, cause: Cause) {
    let sample = diag::sampled();
    let start = node.caller.get().copied();
    let mut cleared = 0u64;
    let mut stack = vec![Arc::clone(node)];
    while let Some(node) = stack.pop() {
        if node.mark.swap(NOT_QUIET, Ordering::AcqRel) != NOT_QUIET {
            INVALIDATED.fetch_add(1, Ordering::Relaxed);
            cleared += 1;
            if sample {
                diag::cleared(node.caller.get().copied());
            }
            stack.extend(node.outputs.lock().iter().filter_map(Weak::upgrade));
        }
    }
    if sample {
        diag::clearing(cause, start, cleared);
    }
}

/// What started a clearing walk.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(super) enum Cause {
    /// A ticket seeded at the node.
    Seed,
    /// A push from a fire found a marked node.
    Push,
    /// An edge declared into the node.
    Edge,
}

/// Sampled clearing walks and validations, by creation site, behind
/// `HYPHAE_SETTLE_STATS_OUT`: one event in [`diag::EVERY`].
pub(super) mod diag {
    use std::{
        panic::Location,
        sync::{
            LazyLock,
            atomic::{AtomicU64, Ordering},
        },
    };

    use parking_lot::Mutex;
    use rustc_hash::FxHashMap;

    use super::Cause;

    pub(crate) const EVERY: u64 = 64;

    type Site = Option<&'static Location<'static>>;

    static ON: LazyLock<bool> = LazyLock::new(|| std::env::var_os("HYPHAE_SETTLE_STATS_OUT").is_some());
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    pub(crate) static SAMPLED_CLEARINGS: AtomicU64 = AtomicU64::new(0);
    pub(crate) static SAMPLED_VALIDATIONS: AtomicU64 = AtomicU64::new(0);

    /// Walk start (cause, site) -> (walks, nodes cleared, longest walk).
    static STARTS: LazyLock<Mutex<FxHashMap<(Cause, usize), (Site, u64, u64, u64)>>> =
        LazyLock::new(Mutex::default);
    /// Cleared site -> times cleared.
    static CLEARED: LazyLock<Mutex<FxHashMap<usize, (Site, u64)>>> = LazyLock::new(Mutex::default);
    /// (reader site, input site) -> (validations, nodes certified, deepest).
    static READS: LazyLock<Mutex<FxHashMap<(usize, usize), (Site, Site, u64, u64, u64)>>> =
        LazyLock::new(Mutex::default);

    fn key(site: Site) -> usize {
        site.map_or(0, |site| std::ptr::from_ref(site).addr())
    }

    pub(crate) fn sampled() -> bool {
        *ON && COUNTER.fetch_add(1, Ordering::Relaxed) % EVERY == 0
    }

    pub(crate) fn cleared(site: Site) {
        let mut cleared = CLEARED.lock();
        let entry = cleared.entry(key(site)).or_insert((site, 0));
        entry.1 += 1;
    }

    pub(crate) fn clearing(cause: Cause, start: Site, length: u64) {
        SAMPLED_CLEARINGS.fetch_add(1, Ordering::Relaxed);
        let mut starts = STARTS.lock();
        let entry = starts.entry((cause, key(start))).or_insert((start, 0, 0, 0));
        entry.1 += 1;
        entry.2 += length;
        entry.3 = entry.3.max(length);
    }

    pub(crate) fn validation(reader: Site, input: Site, depth: u64) {
        SAMPLED_VALIDATIONS.fetch_add(1, Ordering::Relaxed);
        let mut reads = READS.lock();
        let entry = reads
            .entry((key(reader), key(input)))
            .or_insert((reader, input, 0, 0, 0));
        entry.2 += 1;
        entry.3 += depth;
        entry.4 = entry.4.max(depth);
    }

    fn site(site: Site) -> String {
        site.map_or_else(|| "<unknown>".to_owned(), ToString::to_string)
    }

    /// Lines for `<path>.clears`, most nodes cleared first.
    pub(crate) fn clear_lines() -> Vec<String> {
        let mut starts: Vec<_> = STARTS.lock().iter().map(|(k, v)| (k.0, *v)).collect();
        starts.sort_by_key(|(_, v)| std::cmp::Reverse(v.2));
        let mut lines: Vec<String> = vec![format!(
            "# sampled 1/{EVERY}: walks cleared_total longest cause start_site"
        )];
        lines.extend(starts.iter().take(200).map(|(cause, (start, walks, total, longest))| {
            format!("{walks} {total} {longest} {cause:?} {}", site(*start))
        }));
        let mut cleared: Vec<_> = CLEARED.lock().values().copied().collect();
        cleared.sort_by_key(|v| std::cmp::Reverse(v.1));
        lines.push("# cleared site: times".to_owned());
        lines.extend(cleared.iter().take(200).map(|(s, n)| format!("{n} {}", site(*s))));
        lines
    }

    /// Lines for `<path>.validations`, most nodes certified first.
    pub(crate) fn validation_lines() -> Vec<String> {
        let mut reads: Vec<_> = READS.lock().values().copied().collect();
        reads.sort_by_key(|v| std::cmp::Reverse(v.3));
        let mut lines = vec![format!(
            "# sampled 1/{EVERY}: validations certified_total deepest reader_site <- input_site"
        )];
        lines.extend(reads.iter().take(200).map(|(reader, input, n, total, deepest)| {
            format!("{n} {total} {deepest} {} <- {}", site(*reader), site(*input))
        }));
        lines
    }
}

/// Clear a certificate that a push from a fire found. A push along a
/// declared edge from the firing node never finds one, because the firing
/// node is in the target's ancestry and holds the ticket.
pub(super) fn invalidate_if_marked(node: &Arc<NodeCore>) {
    if node.mark.load(Ordering::Acquire) != NOT_QUIET {
        invalidate(node, Cause::Push);
    }
}

/// Certify `node` quiet, ancestors first: nothing in its ancestry holds
/// pending work. Check, mark tentative, check again, then promote; a seed in
/// between clears the tentative mark and the promotion fails.
fn validate(node: &Arc<NodeCore>, seen: &mut FxHashSet<usize>, certified: &mut u64) -> bool {
    if quiet(node) {
        return true;
    }
    if !seen.insert(Arc::as_ptr(node).addr()) {
        return false;
    }
    let ancestors = inputs(node);
    if !ancestors.iter().all(|input| validate(input, seen, certified)) {
        return false;
    }
    let holds = |node: &NodeCore| node.idle() && ancestors.iter().all(|input| quiet(input));
    if !holds(node) {
        return false;
    }
    node.mark.store(TENTATIVE, Ordering::Release);
    if !holds(node) {
        return false;
    }
    let promoted = node
        .mark
        .compare_exchange(TENTATIVE, QUIET, Ordering::AcqRel, Ordering::Acquire)
        .is_ok();
    if promoted {
        VALIDATED.fetch_add(1, Ordering::Relaxed);
        *certified += 1;
    }
    promoted
}

/// Whether `m` has promised past `ticket`, refreshing its watermark from its
/// inputs. On failure, the node found holding work at or below `ticket`.
///
/// Inputs are read before the node's own pending list: an input's watermark
/// passes a ticket only after its push of that ticket landed here.
fn passed(
    m: &Arc<NodeCore>,
    parent: &NodeCore,
    ticket: u64,
    seen: &mut FxHashSet<usize>,
    cost: &mut u64,
) -> Result<(), Arc<NodeCore>> {
    if quiet(m) || m.watermark.load(Ordering::Acquire) > ticket {
        return Ok(());
    }
    if !seen.insert(Arc::as_ptr(m).addr()) {
        // Visited in this walk and not past `ticket`: it is on the current
        // path, so this edge closes a cycle. The design assumes the declared
        // graph is acyclic; a back edge is ignored, as heights ignore it, and
        // counted.
        let seen_cycles = CYCLES.fetch_add(1, Ordering::Relaxed);
        if seen_cycles < 200_000 || seen_cycles.is_multiple_of(1024) {
            record_cycle(m, parent);
        }
        return Ok(());
    }
    *cost = cost.saturating_add(1);
    let mut low = SEEDED.load(Ordering::Acquire);
    if low <= ticket {
        return Err(Arc::clone(m));
    }
    let inputs: Vec<Arc<NodeCore>> = m.inputs.lock().iter().filter_map(Weak::upgrade).collect();
    for input in &inputs {
        passed(input, m, ticket, seen, cost)?;
        low = low.min(input.watermark.load(Ordering::Acquire));
    }
    let pending = m.pending.lock();
    let min = pending.min();
    if min <= ticket {
        return Err(Arc::clone(m));
    }
    let previous = m.watermark.fetch_max(low.min(min), Ordering::AcqRel);
    debug_assert!(
        min >= previous,
        "hyphae: promise safety: watermark {previous} was above pending {min}"
    );
    drop(pending);
    Ok(())
}

/// Record a back edge `from -> to`: `to` lists `from` as an input, and
/// `from` is already on the walk's path.
fn record_cycle(from: &NodeCore, to: &NodeCore) {
    let site = |node: &NodeCore| {
        node.caller
            .get()
            .map_or_else(|| "<unknown>".to_owned(), ToString::to_string)
    };
    let key = (site(from), site(to));
    CYCLE_SITES
        .lock()
        .entry(key)
        .and_modify(|count| *count = count.saturating_add(1))
        .or_insert(1);
}

/// Sampled back edges, most frequent first.
pub(super) fn cycle_sites() -> Vec<((String, String), u64)> {
    let mut sites: Vec<_> = CYCLE_SITES
        .lock()
        .iter()
        .map(|(key, count)| (key.clone(), *count))
        .collect();
    sites.sort_by_key(|entry| std::cmp::Reverse(entry.1));
    sites
}
