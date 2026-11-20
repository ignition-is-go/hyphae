//! Debug check that a callback write which joins the settle travels a
//! declared edge.
//!
//! A node's callbacks may write into the settle in progress only along the
//! dependent edges the graph declares, which `own` and `own_keyed` register.
//! Under pipelined tickets (`docs/settle-epochs.md`, section 6) an undeclared
//! write cannot join its ticket and becomes a new one. This check finds, on
//! today's core, every write that joins without an edge, so the edge can be
//! declared where the written node is built.
//!
//! In debug builds the check panics on such a write. `HYPHAE_EDGE_CHECK=warn`
//! logs it once per pair of creation sites instead, and `off` disables it.
//! Release builds compile none of it. A write that is meant to start a new ticket says so with
//! [`new_ticket`].

/// Run `f`, whose writes from a callback are meant to become their own
/// ticket rather than join the firing node's.
///
/// Today such a write still joins the settle in progress when it can. The
/// marker only exempts it from the edge check.
pub fn new_ticket<R>(f: impl FnOnce() -> R) -> R {
    #[cfg(debug_assertions)]
    {
        imp::NEW_TICKET.set(imp::NEW_TICKET.get().saturating_add(1));
        let _restore = imp::Restore;
        f()
    }
    #[cfg(not(debug_assertions))]
    f()
}

#[cfg(debug_assertions)]
pub use imp::{check_join, firing};

#[cfg(debug_assertions)]
mod imp {
    use std::{
        cell::{Cell, RefCell},
        panic::Location,
        sync::{LazyLock, Weak},
    };

    use parking_lot::Mutex;
    use rustc_hash::FxHashSet;
    use uuid::Uuid;

    use super::super::Node;

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

    static WARNED: LazyLock<Mutex<FxHashSet<(usize, usize)>>> = LazyLock::new(Mutex::default);

    struct Firing {
        node: Weak<dyn Node>,
        /// The ids of every node reachable from `node`, as of the last walk.
        reach: Option<FxHashSet<Uuid>>,
    }

    thread_local! {
        pub(super) static NEW_TICKET: Cell<u32> = const { Cell::new(0) };
        static FIRING: RefCell<Option<Firing>> = const { RefCell::new(None) };
    }

    pub(super) struct Restore;

    impl Drop for Restore {
        fn drop(&mut self) {
            NEW_TICKET.set(NEW_TICKET.get().saturating_sub(1));
        }
    }

    /// Restores the outer firing node when a fire ends.
    struct Clear(Option<Firing>);

    impl Drop for Clear {
        fn drop(&mut self) {
            let previous = self.0.take();
            FIRING.with_borrow_mut(|firing| *firing = previous);
        }
    }

    /// Run `fire` with `node` as the node whose callbacks are writing.
    pub fn firing<R>(node: Weak<dyn Node>, fire: impl FnOnce() -> R) -> R {
        if *MODE == Mode::Off {
            return fire();
        }
        let previous =
            FIRING.with_borrow_mut(|firing| firing.replace(Firing { node, reach: None }));
        let _clear = Clear(previous);
        fire()
    }

    fn reach(start: &dyn Node) -> FxHashSet<Uuid> {
        let mut seen = FxHashSet::default();
        let mut stack = start.node_dependents();
        while let Some(weak) = stack.pop() {
            let Some(node) = weak.upgrade() else { continue };
            if seen.insert(node.node_id()) {
                stack.extend(node.node_dependents());
            }
        }
        seen
    }

    fn site(location: Option<&'static Location<'static>>) -> String {
        location.map_or_else(|| "<unknown>".to_owned(), ToString::to_string)
    }

    /// Check a write to `target` that joins the settle from the node now
    /// firing on this thread.
    pub fn check_join(target: &dyn Node) {
        if *MODE == Mode::Off || NEW_TICKET.get() > 0 {
            return;
        }
        let escape = FIRING.with_borrow_mut(|firing| {
            let firing = firing.as_mut()?;
            let node = firing.node.upgrade()?;
            let id = target.node_id();
            if firing
                .reach
                .as_ref()
                .is_some_and(|reach| reach.contains(&id))
            {
                return None;
            }
            // An edge declared since the last walk, by a node built while
            // this one fires, counts.
            let fresh = reach(node.as_ref());
            let declared = fresh.contains(&id);
            firing.reach = Some(fresh);
            (!declared).then(|| node.node_caller())
        });
        let Some(from) = escape else { return };
        let to = target.node_caller();
        let message = format!(
            "hyphae: a callback of the node created at {} wrote the node created at {}, \
             which has no declared edge from it",
            site(from),
            site(to)
        );
        match *MODE {
            Mode::Off => {}
            Mode::Warn => {
                let key = (
                    from.map_or(0, |l| std::ptr::from_ref(l).addr()),
                    to.map_or(0, |l| std::ptr::from_ref(l).addr()),
                );
                if WARNED.lock().insert(key) {
                    log::warn!("{message}");
                    eprintln!("{message}");
                }
            }
            #[allow(clippy::panic, reason = "the check is opt-in and debug-only")]
            Mode::Panic => panic!("{message}"),
        }
    }
}
