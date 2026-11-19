//! The run scheduler: which runs may settle now.
//!
//! Every run takes a ticket in arrival order and reserves the nodes it may
//! reach, its cone. Each node keeps a line of the tickets that reserved it,
//! oldest first, and serves the front of its line. A run starts once it is at
//! the front of every line it stands in, so two runs that share a node settle
//! one after the other in ticket order and runs with disjoint cones settle at
//! the same time.
//!
//! A run can reach a node outside its cone while it settles (an edge the cone
//! walk does not see, or one added during the run). That is a cone escape.
//! The run takes the node only when no other run can have been served there
//! out of order: the line is empty, or only younger runs that have not
//! started stand in it. Otherwise the write must not join the run, and the
//! caller defers it to a later run with a later ticket.
//!
//! This file holds no lock and no runtime type, so the loom model in
//! `tests/loom_sched.rs` checks this exact code. `docs/settle-core.md`
//! section 13 has the argument.

use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    hash::Hash,
};

use rustc_hash::FxHashMap;

pub type Ticket = u64;

/// What a run that escapes its cone does with the node it reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Escape {
    /// The node is now reserved for the run. The write joins it.
    Take,
    /// Another run holds the node or is owed it first. The write must go to
    /// a later run.
    Defer,
}

struct Slot<K> {
    keys: Vec<K>,
    /// Lines in which another ticket stands at the front.
    blocked: usize,
    barrier: bool,
    started: bool,
}

pub struct Sched<K> {
    next: Ticket,
    lines: FxHashMap<K, VecDeque<Ticket>>,
    runs: BTreeMap<Ticket, Slot<K>>,
    ready: BTreeSet<Ticket>,
    barriers: BTreeSet<Ticket>,
    /// The youngest ticket each node has served, kept while some live run
    /// is older. A run that escapes into a node a younger run has already
    /// left must not be served there after it.
    served: FxHashMap<K, Ticket>,
}

impl<K: Copy + Eq + Hash> Default for Sched<K> {
    fn default() -> Self {
        Self {
            next: 1,
            lines: FxHashMap::default(),
            runs: BTreeMap::new(),
            ready: BTreeSet::new(),
            barriers: BTreeSet::new(),
            served: FxHashMap::default(),
        }
    }
}

impl<K: Copy + Eq + Hash> Sched<K> {
    /// Give a run the next ticket and stand it at the back of the line of
    /// each node in `keys`. A barrier settles alone: it starts after every
    /// older run has finished, and no younger run starts before it finishes.
    pub fn admit(&mut self, keys: impl IntoIterator<Item = K>, barrier: bool) -> Ticket {
        let ticket = self.next;
        self.next = self.next.wrapping_add(1);
        let mut slot = Slot {
            keys: Vec::new(),
            blocked: 0,
            barrier,
            started: false,
        };
        for key in keys {
            let line = self.lines.entry(key).or_default();
            if line.back() == Some(&ticket) {
                continue;
            }
            if !line.is_empty() {
                slot.blocked = slot.blocked.saturating_add(1);
            }
            line.push_back(ticket);
            slot.keys.push(key);
        }
        if barrier {
            self.barriers.insert(ticket);
        }
        self.runs.insert(ticket, slot);
        self.refresh(ticket);
        ticket
    }

    /// Start the oldest run that may start now.
    pub fn start(&mut self) -> Option<Ticket> {
        let ticket = self.ready.pop_first()?;
        if let Some(slot) = self.runs.get_mut(&ticket) {
            slot.started = true;
        }
        Some(ticket)
    }

    /// Runs that may start now.
    pub fn ready(&self) -> usize {
        self.ready.len()
    }

    /// Runs admitted and not finished.
    pub fn live(&self) -> usize {
        self.runs.len()
    }

    /// The started run `ticket` reached `key`, which it did not reserve.
    pub fn escape(&mut self, ticket: Ticket, key: K) -> Escape {
        if self.served.get(&key).is_some_and(|served| *served > ticket) {
            return Escape::Defer;
        }
        let line = self.lines.entry(key).or_default();
        match line.front().copied() {
            Some(front) if front == ticket => return Escape::Take,
            // An older run is owed the node first.
            Some(front) if front < ticket => return Escape::Defer,
            Some(front) if self.runs.get(&front).is_some_and(|slot| slot.started) => {
                return Escape::Defer;
            }
            _ => {}
        }
        // Every ticket in the line is younger and none has started.
        let displaced = line.front().copied();
        line.push_front(ticket);
        if let Some(slot) = self.runs.get_mut(&ticket) {
            slot.keys.push(key);
        }
        if let Some(front) = displaced
            && let Some(slot) = self.runs.get_mut(&front)
        {
            slot.blocked = slot.blocked.saturating_add(1);
            self.ready.remove(&front);
        }
        Escape::Take
    }

    /// The started run `ticket` has settled. Leave each line it stood in.
    pub fn finish(&mut self, ticket: Ticket) {
        let Some(slot) = self.runs.remove(&ticket) else {
            return;
        };
        let mut fronts = Vec::new();
        for key in slot.keys {
            let Some(line) = self.lines.get_mut(&key) else {
                continue;
            };
            debug_assert_eq!(line.front(), Some(&ticket), "a run left a line it did not head");
            line.pop_front();
            self.served.insert(key, ticket);
            match line.front().copied() {
                Some(front) => fronts.push(front),
                None => {
                    self.lines.remove(&key);
                }
            }
        }
        for front in fronts {
            if let Some(slot) = self.runs.get_mut(&front) {
                slot.blocked = slot.blocked.saturating_sub(1);
            }
            self.refresh(front);
        }
        if slot.barrier {
            self.barriers.remove(&ticket);
        }
        // The oldest live run may now be a barrier with nothing left before
        // it, and a finished barrier frees the runs behind it.
        let behind = self.barriers.first().copied().unwrap_or(Ticket::MAX);
        let candidates: Vec<Ticket> = if slot.barrier {
            self.runs.range(..=behind).map(|(ticket, _)| *ticket).collect()
        } else {
            self.runs.keys().next().copied().into_iter().collect()
        };
        for candidate in candidates {
            self.refresh(candidate);
        }
        if self.served.len() > 64 && self.served.len() > self.lines.len().saturating_mul(2) {
            // Only a live run can escape, and no live run is older than this.
            let oldest = self.runs.keys().next().copied().unwrap_or(Ticket::MAX);
            self.served.retain(|_, served| *served > oldest);
        }
    }

    fn refresh(&mut self, ticket: Ticket) {
        let Some(slot) = self.runs.get(&ticket) else {
            return;
        };
        let older_barrier = self.barriers.first().is_some_and(|barrier| *barrier < ticket);
        let oldest = self.runs.keys().next() == Some(&ticket);
        if !slot.started && slot.blocked == 0 && !older_barrier && (!slot.barrier || oldest) {
            self.ready.insert(ticket);
        }
    }
}
