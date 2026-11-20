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
