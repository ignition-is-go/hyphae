//! Loom model of the run scheduler in `src/settle/sched.rs`.
//!
//! Each writer thread admits one run, then settles any run that is ready, as
//! the settle core does. A run walks a fixed list of nodes in height order;
//! a node outside its cone is a cone escape. A deferred escape becomes a new
//! run with a later ticket. Loom explores every interleaving and checks:
//!
//! - G4: every node serves runs in strictly increasing ticket order;
//! - fanouts that share a node never interleave: one run at a time at a node;
//! - glitch-free joins: when the join fires in a run, no input it shares with
//!   that run was last written by a run still settling;
//! - no deadlock: every thread ends and every admitted run finishes.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::significant_drop_tightening
)]

#[path = "../src/settle/sched.rs"]
#[allow(dead_code, unreachable_pub)]
mod sched;

use loom::{
    sync::{Arc, Condvar, Mutex},
    thread,
};
use sched::{Escape, Sched, Ticket};

const JOIN: usize = 3;
const NODES: usize = 4;

/// One run: the nodes it reserves and the nodes it fires, in height order.
#[derive(Clone)]
struct Plan {
    cone: Vec<usize>,
    fires: Vec<usize>,
    barrier: bool,
}

#[derive(Default)]
struct NodeState {
    holder: Option<Ticket>,
    served: Vec<Ticket>,
    /// The run that last wrote this node's value, for the join check.
    written_by: Option<Ticket>,
}

struct World {
    sched: Mutex<(Sched<usize>, Vec<(Ticket, Plan)>, Vec<Ticket>)>,
    nodes: Vec<Mutex<NodeState>>,
    changed: Condvar,
}

impl World {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            sched: Mutex::new((Sched::default(), Vec::new(), Vec::new())),
            nodes: (0..NODES)
                .map(|_| Mutex::new(NodeState::default()))
                .collect(),
            changed: Condvar::new(),
        })
    }

    fn admit(&self, plan: Plan) {
        let mut guard = self.sched.lock().unwrap();
        let ticket = guard.0.admit(plan.cone.iter().copied(), plan.barrier);
        guard.1.push((ticket, plan));
        drop(guard);
        self.changed.notify_all();
    }

    /// Settle one ready run. False when none is ready.
    fn settle_one(&self) -> bool {
        let (ticket, plan) = {
            let mut guard = self.sched.lock().unwrap();
            let Some(ticket) = guard.0.start() else {
                return false;
            };
            let plan = guard
                .1
                .iter()
                .find(|(t, _)| *t == ticket)
                .map(|(_, plan)| plan.clone())
                .unwrap();
            (ticket, plan)
        };
        let mut held: Vec<usize> = Vec::new();
        let mut deferred: Vec<usize> = Vec::new();
        for &node in &plan.fires {
            if !plan.barrier && !plan.cone.contains(&node) {
                let escape = self.sched.lock().unwrap().0.escape(ticket, node);
                if escape == Escape::Defer {
                    deferred.push(node);
                    continue;
                }
            }
            self.fire(ticket, node, &plan);
            held.push(node);
        }
        for node in held {
            let mut state = self.nodes[node].lock().unwrap();
            if state.holder == Some(ticket) {
                state.holder = None;
            }
        }
        let mut guard = self.sched.lock().unwrap();
        if !deferred.is_empty() {
            let plan = Plan {
                cone: deferred.clone(),
                fires: deferred,
                barrier: false,
            };
            let next = guard.0.admit(plan.cone.iter().copied(), false);
            guard.1.push((next, plan));
        }
        guard.0.finish(ticket);
        guard.2.push(ticket);
        drop(guard);
        self.changed.notify_all();
        true
    }

    fn fire(&self, ticket: Ticket, node: usize, plan: &Plan) {
        if node == JOIN {
            for input in 0..JOIN {
                if plan.cone.contains(&input) || plan.barrier {
                    let state = self.nodes[input].lock().unwrap();
                    if let Some(writer) = state.written_by {
                        let done = self.sched.lock().unwrap().2.contains(&writer);
                        assert!(
                            writer == ticket || done,
                            "join fired in run {ticket} over input {input} of unfinished run {writer}"
                        );
                    }
                }
            }
        }
        let mut state = self.nodes[node].lock().unwrap();
        assert!(
            state.holder.is_none_or(|holder| holder == ticket),
            "runs {:?} and {ticket} interleave at node {node}",
            state.holder
        );
        if let Some(last) = state.served.last() {
            assert!(
                *last <= ticket,
                "node {node} served run {ticket} after run {last}"
            );
        }
        state.holder = Some(ticket);
        state.served.push(ticket);
        state.written_by = Some(ticket);
    }

    /// Settle ready runs until every run has finished.
    fn drive(&self) {
        loop {
            if self.settle_one() {
                continue;
            }
            let guard = self.sched.lock().unwrap();
            if guard.0.live() == 0 {
                return;
            }
            if guard.0.ready() == 0 {
                drop(self.changed.wait(guard).unwrap());
            }
        }
    }
}

fn check(plans: Vec<Plan>) {
    let mut builder = loom::model::Builder::new();
    builder.preemption_bound = Some(3);
    builder.check(move || {
        let world = World::new();
        let threads: Vec<_> = plans
            .clone()
            .into_iter()
            .map(|plan| {
                let world = world.clone();
                thread::spawn(move || {
                    world.admit(plan);
                    world.drive();
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        let guard = world.sched.lock().unwrap();
        assert_eq!(guard.0.live(), 0, "a run never finished");
        assert_eq!(guard.0.ready(), 0);
        assert_eq!(guard.2.len(), guard.1.len(), "every admitted run finished");
    });
}

fn plan(cone: &[usize], fires: &[usize]) -> Plan {
    Plan {
        cone: cone.to_vec(),
        fires: fires.to_vec(),
        barrier: false,
    }
}

/// Two inputs of one join, plus a run whose edge into the join is invisible
/// to the cone walk and escapes into it.
#[test]
fn overlapping_runs_and_an_escape_into_a_join() {
    check(vec![
        plan(&[0, JOIN], &[0, JOIN]),
        plan(&[1, JOIN], &[1, JOIN]),
        plan(&[2], &[2, JOIN]),
    ]);
}

/// A run escapes into a node a younger run reserved, and the younger run
/// escapes into the older run's node.
#[test]
fn crossing_escapes() {
    check(vec![plan(&[0], &[0, 1]), plan(&[1], &[1, 0])]);
}

/// A barrier settles alone between ordinary runs.
#[test]
fn a_barrier_between_runs() {
    check(vec![
        plan(&[0], &[0, JOIN]),
        Plan {
            cone: Vec::new(),
            fires: vec![1, JOIN],
            barrier: true,
        },
        plan(&[2, JOIN], &[2, JOIN]),
    ]);
}
