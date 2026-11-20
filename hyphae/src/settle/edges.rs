//! Diagnostics for writes that cannot join their ticket, and the marker for
//! writes that are meant to start a new one.
//!
//! The check panics on a callback write with no declared edge and on an
//! owned subscription that cannot be bound to its owner.
//! `HYPHAE_EDGE_CHECK=warn` logs each once per creation site instead, and
//! `off` disables it.

use std::{
    cell::Cell,
    panic::Location,
    sync::{LazyLock, atomic::Ordering},
};

use parking_lot::Mutex;
use rustc_hash::{FxHashMap, FxHashSet};

use super::Node;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Off,
    Warn,
    Panic,
}

static MODE: LazyLock<Mode> =
    LazyLock::new(|| match std::env::var("HYPHAE_EDGE_CHECK").as_deref() {
        Ok("off") => Mode::Off,
        Ok("warn") => Mode::Warn,
        _ => Mode::Panic,
    });

static WARNED: LazyLock<Mutex<FxHashSet<(u8, usize)>>> = LazyLock::new(Mutex::default);

/// Owned subscriptions that could not be bound, per owner creation site.
static UNBOUND_SITES: LazyLock<Mutex<FxHashMap<String, u64>>> = LazyLock::new(Mutex::default);

thread_local! {
    static NEW_TICKET: Cell<u32> = const { Cell::new(0) };
}

/// Run `f`, whose writes from a callback become their own ticket rather
/// than join the firing node's.
pub fn new_ticket<R>(f: impl FnOnce() -> R) -> R {
    struct Restore;
    impl Drop for Restore {
        fn drop(&mut self) {
            NEW_TICKET.set(NEW_TICKET.get().saturating_sub(1));
        }
    }
    NEW_TICKET.set(NEW_TICKET.get().saturating_add(1));
    let _restore = Restore;
    f()
}

pub(super) fn forced() -> bool {
    NEW_TICKET.get() > 0
}

fn site(location: Option<&'static Location<'static>>) -> String {
    location.map_or_else(|| "<unknown>".to_owned(), ToString::to_string)
}

fn once(kind: u8, location: Option<&'static Location<'static>>) -> bool {
    let key = location.map_or(0, |l| std::ptr::from_ref(l).addr());
    WARNED.lock().insert((kind, key))
}

/// A callback wrote `target` with no declared edge from the firing node.
pub(super) fn undeclared(target: &dyn Node) {
    let mode = *MODE;
    if mode == Mode::Off {
        return;
    }
    let message = format!(
        "hyphae: a callback wrote the node created at {} with no declared edge from the firing node",
        site(target.node_caller())
    );
    assert!(mode != Mode::Panic, "{message}");
    if once(0, target.node_caller()) {
        log::warn!("{message}");
        eprintln!("{message}");
    }
}

/// The lifecycle rule: a node is created only between tickets, as a root on
/// a thread outside any settle, or inside a family's fire, as a row. A node
/// created in a callback during a ticket is a construction error.
///
/// `HYPHAE_LIFECYCLE=report` records each offending site, with the first
/// stack frame outside hyphae and the node whose fire was running, in
/// `$HYPHAE_LIFECYCLE_OUT` (once per site); `panic` panics with the same
/// sites; `off` disables the check.
pub(super) fn lifecycle(
    caller: &'static Location<'static>,
    during: Option<&'static Location<'static>>,
) {
    static MODE: LazyLock<u8> =
        // TODO: panic is the default once hyphae's own sites (the per-entry
        // mappers of project_cell, select_cell and map_values_cell) are
        // family rows. Until then report is the default.
        LazyLock::new(|| match std::env::var("HYPHAE_LIFECYCLE").as_deref() {
            Ok("off") => 0,
            Ok("panic") => 2,
            _ => 1,
        });
    match *MODE {
        0 => {}
        1 => lifecycle_report(caller, during),
        _ => panic!(
            "hyphae: lifecycle rule: a node was created at {caller} (called from {}) inside \
             a callback of the node created at {} during a ticket; build it between tickets, \
             or as a family row",
            outside_frame(),
            site(during)
        ),
    }
}

/// The first stack frame outside hyphae and the standard library.
fn outside_frame() -> String {
    let trace = std::backtrace::Backtrace::force_capture().to_string();
    let mut lines = trace.lines();
    while let Some(line) = lines.next() {
        let line = line.trim();
        if let Some(path) = line.strip_prefix("at ")
            && !path.contains("/hyphae/src/")
            && !path.starts_with("./src/")
            && !path.contains("/rustc/")
            && !path.contains("/rustlib/")
            && !path.contains("/.cargo/registry/")
        {
            return path.to_owned();
        }
    }
    "<no frame outside hyphae>".to_owned()
}

fn lifecycle_report(caller: &'static Location<'static>, during: Option<&'static Location<'static>>) {
    static OUT: LazyLock<Option<String>> = LazyLock::new(|| std::env::var("HYPHAE_LIFECYCLE_OUT").ok());
    static SEEN: LazyLock<Mutex<FxHashSet<(usize, usize)>>> = LazyLock::new(Mutex::default);
    let Some(path) = OUT.as_ref() else { return };
    // Once per (creation site, firing site): the backtrace is costly.
    let key = (
        std::ptr::from_ref(caller).addr(),
        during.map_or(0, |during| std::ptr::from_ref(during).addr()),
    );
    if !SEEN.lock().insert(key) {
        return;
    }
    let line = format!("{caller} <- {} | in the fire of {}", outside_frame(), site(during));
    if let Ok(mut out) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        use std::io::Write as _;
        let _ = writeln!(out, "{line}");
    }
}

/// A write along an edge declared during the ticket reached a node already
/// past it, and became a new ticket. Logged once per creation site.
pub(super) fn late(target: &dyn Node) {
    if *MODE != Mode::Off && once(1, target.node_caller()) {
        let message = format!(
            "hyphae: late edge: a write to the node created at {} became a new ticket; \
             its edge was declared during a ticket",
            site(target.node_caller())
        );
        log::warn!("{message}");
        eprintln!("{message}");
    }
}

/// An owned subscription of the owner created at `owner` could not be bound
/// to it, so its callback runs at the source's fire. A known hole: operator
/// state shared across inputs is then not ordered by ticket.
pub fn unbound_owned(owner: Option<&'static Location<'static>>) {
    super::UNBOUND_OWNED.fetch_add(1, Ordering::Relaxed);
    let name = site(owner);
    UNBOUND_SITES
        .lock()
        .entry(name.clone())
        .and_modify(|count| *count = count.saturating_add(1))
        .or_insert(1);
    assert!(
        *MODE != Mode::Panic,
        "hyphae: an owned subscription of the node created at {name} cannot be bound to its owner"
    );
}

/// Owned subscriptions that could not be bound, by owner creation site.
#[must_use]
pub fn unbound_sites() -> Vec<(String, u64)> {
    let mut sites: Vec<_> = UNBOUND_SITES
        .lock()
        .iter()
        .map(|(site, count)| (site.clone(), *count))
        .collect();
    sites.sort();
    sites
}
