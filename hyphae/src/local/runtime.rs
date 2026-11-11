use std::{
    any::Any,
    collections::VecDeque,
    marker::PhantomData,
    panic::{AssertUnwindSafe, catch_unwind, resume_unwind},
    sync::Arc,
};

use rustc_hash::FxHashMap;

use super::{
    Builder, Cell, Error, Id, Input, Lease, Observer, Result, RetireQueue, Retired, ScopeId, Stats,
    Transaction, Value, WatchLease, Write,
    body::{Body, Switch, Typed},
};

type Callback = Box<dyn FnMut(&dyn Any, &mut Transaction) -> Result<()> + Send>;
struct Observation {
    node: Arc<Lease>,
    callback: Option<Callback>,
    last: Option<u64>,
}
#[allow(
    clippy::struct_excessive_bools,
    reason = "validity, demand, traversal and queue membership are independent graph invariants"
)]
struct Node {
    lease: std::sync::Weak<Lease>,
    body: Option<Box<dyn Body>>,
    inputs: Vec<Arc<Lease>>,
    parents: Vec<Id>,
    observers: Vec<u64>,
    effects: Vec<u64>,
    demand: usize,
    dirty: bool,
    visiting: bool,
    valid: bool,
    queued: bool,
    switch: bool,
    changed: u64,
    computed: u64,
}
struct Slot {
    generation: u64,
    node: Option<Node>,
}
struct ScopeSlot {
    generation: u64,
    nodes: Option<Vec<Id>>,
}
#[derive(Clone, Copy)]
enum Work {
    Visit(Id),
    Select(Id),
    Finish(Id),
}
#[derive(Clone, Copy, Eq, PartialEq)]
enum Phase {
    Idle,
    Computing,
    Publishing,
    Poisoned,
}

/// Single-owner graph storage. It can move to another thread, but cannot be shared.
/// No scheduler, clock, thread pool or process-global graph state is installed.
pub struct Runtime {
    owner: Arc<()>,
    retired: Arc<RetireQueue>,
    slots: Vec<Slot>,
    free: Vec<usize>,
    scopes: Vec<ScopeSlot>,
    free_scopes: Vec<usize>,
    pub(super) current_scope: Option<ScopeId>,
    observations: FxHashMap<u64, Observation>,
    next_watch: u64,
    epoch: u64,
    phase: Phase,
    targets: Vec<Id>,
    work: Vec<Work>,
    dirty_work: Vec<Id>,
    feedback: Vec<Write>,
    stats: Stats,
}
impl Default for Runtime {
    fn default() -> Self {
        Self::new()
    }
}
impl Runtime {
    #[must_use]
    pub fn new() -> Self {
        Self {
            owner: Arc::new(()),
            retired: Arc::default(),
            slots: Vec::new(),
            free: Vec::new(),
            scopes: Vec::new(),
            free_scopes: Vec::new(),
            current_scope: None,
            observations: FxHashMap::default(),
            next_watch: 0,
            epoch: 0,
            phase: Phase::Idle,
            targets: Vec::new(),
            work: Vec::new(),
            dirty_work: Vec::new(),
            feedback: Vec::new(),
            stats: Stats::default(),
        }
    }
    #[must_use]
    pub const fn stats(&self) -> Stats {
        self.stats
    }
    #[must_use]
    pub const fn has_feedback(&self) -> bool {
        !self.feedback.is_empty()
    }
    const fn idle(&self) -> Result<()> {
        match self.phase {
            Phase::Idle => Ok(()),
            Phase::Poisoned => Err(Error::Poisoned),
            _ => Err(Error::Busy),
        }
    }
    fn node(&self, id: Id) -> Result<&Node> {
        self.slots
            .get(id.slot)
            .filter(|s| s.generation == id.generation)
            .and_then(|s| s.node.as_ref())
            .ok_or(Error::InvalidHandle)
    }
    fn node_mut(&mut self, id: Id) -> Result<&mut Node> {
        self.slots
            .get_mut(id.slot)
            .filter(|s| s.generation == id.generation)
            .and_then(|s| s.node.as_mut())
            .ok_or(Error::InvalidHandle)
    }
    pub(super) fn check<T: Value>(&self, cell: &Cell<T>) -> Result<Id> {
        if !Arc::ptr_eq(&self.owner, &cell.lease.owner) {
            return Err(Error::ForeignRuntime);
        }
        let id = cell.lease.id;
        if !self.node(id)?.valid {
            return Err(Error::InvalidScope);
        }
        Ok(id)
    }
    pub(super) fn value<T: Value>(&self, id: Id) -> Result<&T> {
        let node = self.node(id)?;
        if !node.valid {
            return Err(Error::InvalidScope);
        }
        node.body
            .as_ref()
            .and_then(|b| b.value())
            .ok_or(Error::Unsettled)?
            .downcast_ref::<T>()
            .ok_or(Error::TypeMismatch)
    }
    pub(super) fn read<T: Value>(&self, cell: &Cell<T>) -> Result<T> {
        self.idle()?;
        let id = self.check(cell)?;
        if self.node(id)?.dirty {
            return Err(Error::Unsettled);
        }
        self.value(id).cloned()
    }
    fn install<T: Value>(
        &mut self,
        body: Box<dyn Body>,
        inputs: Vec<Arc<Lease>>,
        switch: bool,
        settled: bool,
    ) -> Result<Cell<T>> {
        for input in &inputs {
            if !self.node(input.id)?.valid {
                return Err(Error::InvalidScope);
            }
        }
        let slot = self.free.pop().unwrap_or(self.slots.len());
        if slot == self.slots.len() {
            self.slots.push(Slot {
                generation: 0,
                node: None,
            });
        }
        let generation = self.slots.get(slot).ok_or(Error::InvalidHandle)?.generation;
        let id = Id { slot, generation };
        let lease = Arc::new(Lease {
            id,
            owner: self.owner.clone(),
            retired: Arc::downgrade(&self.retired),
        });
        let node = Node {
            lease: Arc::downgrade(&lease),
            body: Some(body),
            inputs,
            parents: Vec::new(),
            observers: Vec::new(),
            effects: Vec::new(),
            demand: 0,
            dirty: !settled,
            visiting: false,
            valid: true,
            queued: false,
            switch,
            changed: self.epoch,
            computed: self.epoch,
        };
        let input_ids: Vec<_> = node.inputs.iter().map(|x| x.id).collect();
        self.slots.get_mut(slot).ok_or(Error::InvalidHandle)?.node = Some(node);
        for input in input_ids {
            self.node_mut(input)?.parents.push(id);
        }
        if let Some(scope) = self.current_scope {
            self.scopes
                .get_mut(scope.slot)
                .filter(|s| s.generation == scope.generation)
                .and_then(|s| s.nodes.as_mut())
                .ok_or(Error::InvalidScope)?
                .push(id);
        }
        self.stats.live_nodes = self.stats.live_nodes.saturating_add(1);
        self.stats.allocated_nodes = self.stats.allocated_nodes.saturating_add(1);
        Ok(Cell {
            lease,
            marker: PhantomData,
        })
    }
    /// # Errors
    /// Returns `Busy` or `Poisoned` when construction is not allowed.
    pub fn input<T: Value>(&mut self, value: T) -> Result<Input<T>> {
        self.idle()?;
        let cell = self.install(
            Box::new(Typed {
                value: Some(value),
                compute: None,
                input: true,
            }),
            Vec::new(),
            false,
            true,
        )?;
        Ok(Input { cell })
    }
    /// # Errors
    /// Returns `Busy` or `Poisoned` when construction is not allowed.
    pub fn constant<T: Value>(&mut self, value: T) -> Result<Cell<T>> {
        self.idle()?;
        self.make_constant(value)
    }
    pub(super) fn make_constant<T: Value>(&mut self, value: T) -> Result<Cell<T>> {
        self.install(
            Box::new(Typed {
                value: Some(value),
                compute: None,
                input: false,
            }),
            Vec::new(),
            false,
            true,
        )
    }
    /// # Errors
    /// Returns an error for a foreign or invalid input, or a non-idle runtime.
    pub fn map<T: Value, U: Value>(
        &mut self,
        input: &Cell<T>,
        f: impl Fn(&T) -> U + Send + 'static,
    ) -> Result<Cell<U>> {
        self.idle()?;
        self.make_map(input, f)
    }
    pub(super) fn make_map<T: Value, U: Value>(
        &mut self,
        input: &Cell<T>,
        f: impl Fn(&T) -> U + Send + 'static,
    ) -> Result<Cell<U>> {
        let id = self.check(input)?;
        self.install(
            Box::new(Typed {
                value: None,
                compute: Some(Box::new(move |rt| Ok(f(rt.value::<T>(id)?)))),
                input: false,
            }),
            vec![input.lease.clone()],
            false,
            false,
        )
    }
    /// # Errors
    /// Returns an error for foreign or invalid inputs, or a non-idle runtime.
    pub fn map2<A: Value, B: Value, U: Value>(
        &mut self,
        a: &Cell<A>,
        b: &Cell<B>,
        f: impl Fn(&A, &B) -> U + Send + 'static,
    ) -> Result<Cell<U>> {
        self.idle()?;
        self.make_map2(a, b, f)
    }
    pub(super) fn make_map2<A: Value, B: Value, U: Value>(
        &mut self,
        a: &Cell<A>,
        b: &Cell<B>,
        f: impl Fn(&A, &B) -> U + Send + 'static,
    ) -> Result<Cell<U>> {
        let ai = self.check(a)?;
        let bi = self.check(b)?;
        self.install(
            Box::new(Typed {
                value: None,
                compute: Some(Box::new(move |rt| {
                    Ok(f(rt.value::<A>(ai)?, rt.value::<B>(bi)?))
                })),
                input: false,
            }),
            vec![a.lease.clone(), b.lease.clone()],
            false,
            false,
        )
    }
    /// # Errors
    /// Returns an error for foreign or invalid inputs, or a non-idle runtime.
    pub fn fold<T: Value, U: Value>(
        &mut self,
        inputs: &[Cell<T>],
        initial: U,
        f: impl Fn(U, &T) -> U + Send + 'static,
    ) -> Result<Cell<U>> {
        self.idle()?;
        self.make_fold(inputs, initial, f)
    }
    pub(super) fn make_fold<T: Value, U: Value>(
        &mut self,
        inputs: &[Cell<T>],
        initial: U,
        f: impl Fn(U, &T) -> U + Send + 'static,
    ) -> Result<Cell<U>> {
        let ids: Vec<_> = inputs
            .iter()
            .map(|x| self.check(x))
            .collect::<Result<_>>()?;
        let leases = inputs.iter().map(|x| x.lease.clone()).collect();
        self.install(
            Box::new(Typed {
                value: None,
                compute: Some(Box::new(move |rt| {
                    let mut value = initial.clone();
                    for id in &ids {
                        value = f(value, rt.value::<T>(*id)?);
                    }
                    Ok(value)
                })),
                input: false,
            }),
            leases,
            false,
            false,
        )
    }
    /// Stateful computation consumes each distinct input value in transaction-step order.
    /// # Errors
    /// Returns an error for a foreign or invalid input, or a non-idle runtime.
    pub fn scan<T: Value, U: Value>(
        &mut self,
        input: &Cell<T>,
        initial: U,
        f: impl FnMut(&U, &T) -> U + Send + 'static,
    ) -> Result<Cell<U>> {
        self.idle()?;
        self.make_scan(input, initial, f)
    }
    pub(super) fn make_scan<T: Value, U: Value>(
        &mut self,
        input: &Cell<T>,
        initial: U,
        mut f: impl FnMut(&U, &T) -> U + Send + 'static,
    ) -> Result<Cell<U>> {
        let id = self.check(input)?;
        let mut state = initial;
        self.install(
            Box::new(Typed {
                value: None,
                compute: Some(Box::new(move |rt| {
                    state = f(&state, rt.value::<T>(id)?);
                    Ok(state.clone())
                })),
                input: false,
            }),
            vec![input.lease.clone()],
            false,
            false,
        )
    }
    /// # Errors
    /// Returns an error for a foreign or invalid selector, or a non-idle runtime.
    /// Factory and cycle errors are reported when the branch is evaluated.
    pub fn switch_map<T: Value, U: Value>(
        &mut self,
        input: &Cell<T>,
        f: impl Fn(&mut Builder<'_>, &T) -> Result<Cell<U>> + Send + 'static,
    ) -> Result<Cell<U>> {
        self.idle()?;
        self.make_switch(input, f)
    }
    pub(super) fn make_switch<T: Value, U: Value>(
        &mut self,
        input: &Cell<T>,
        f: impl Fn(&mut Builder<'_>, &T) -> Result<Cell<U>> + Send + 'static,
    ) -> Result<Cell<U>> {
        let id = self.check(input)?;
        self.install(
            Box::new(Switch {
                outer: id,
                choose: Box::new(f),
                last: None,
                selected: None,
                selected_scope: None,
                value: None,
            }),
            vec![input.lease.clone()],
            true,
            false,
        )
    }
    /// Demand an output without installing an external effect.
    /// # Errors
    /// Returns an error for a foreign or invalid cell, or a non-idle runtime.
    pub fn observe<T: Value>(&mut self, cell: &Cell<T>) -> Result<Observer<T>> {
        self.observe_inner(cell, None)
    }
    /// Effects run only after all demanded outputs in a step have settled.
    /// Writes to `next` become input to the next explicit transaction.
    /// # Errors
    /// Returns an error for a foreign or invalid cell, or a non-idle runtime.
    pub fn subscribe<T: Value>(
        &mut self,
        cell: &Cell<T>,
        mut f: impl FnMut(&T, &mut Transaction) -> Result<()> + Send + 'static,
    ) -> Result<Observer<T>> {
        self.observe_inner(
            cell,
            Some(Box::new(move |v, next| {
                f(v.downcast_ref::<T>().ok_or(Error::TypeMismatch)?, next)
            })),
        )
    }
    fn observe_inner<T: Value>(
        &mut self,
        cell: &Cell<T>,
        callback: Option<Callback>,
    ) -> Result<Observer<T>> {
        self.idle()?;
        self.collect_retired();
        let id = self.check(cell)?;
        let watch = self.next_watch;
        self.next_watch = self
            .next_watch
            .checked_add(1)
            .ok_or(Error::EpochExhausted)?;
        let effect = callback.is_some();
        self.observations.insert(
            watch,
            Observation {
                node: cell.lease.clone(),
                callback,
                last: None,
            },
        );
        self.node_mut(id)?.observers.push(watch);
        if effect {
            self.node_mut(id)?.effects.push(watch);
        }
        self.activate(id);
        self.queue_target(id);
        Ok(Observer {
            cell: cell.clone(),
            _watch: Arc::new(WatchLease {
                id: watch,
                retired: Arc::downgrade(&self.retired),
            }),
        })
    }
    fn activate(&mut self, id: Id) {
        let mut work = vec![id];
        while let Some(id) = work.pop() {
            if let Ok(node) = self.node_mut(id) {
                if !node.valid {
                    continue;
                }
                node.demand = node.demand.saturating_add(1);
                if node.demand == 1 {
                    node.dirty = true;
                    work.extend(node.inputs.iter().map(|i| i.id));
                }
            }
        }
    }
    fn deactivate(&mut self, id: Id) {
        let mut work = vec![id];
        while let Some(id) = work.pop() {
            if let Ok(node) = self.node_mut(id) {
                if node.demand == 0 {
                    continue;
                }
                node.demand = node.demand.saturating_sub(1);
                if node.demand == 0 {
                    work.extend(node.inputs.iter().map(|i| i.id));
                }
            }
        }
    }
    fn queue_target(&mut self, id: Id) {
        if let Ok(node) = self.node_mut(id)
            && node.valid
            && !node.queued
            && !node.observers.is_empty()
        {
            node.queued = true;
            self.targets.push(id);
        }
    }
    fn dirty(&mut self, id: Id) {
        let mut work = std::mem::take(&mut self.dirty_work);
        work.push(id);
        while let Some(id) = work.pop() {
            let Ok(node) = self.node_mut(id) else {
                continue;
            };
            if !node.valid || node.demand == 0 || node.dirty {
                continue;
            }
            node.dirty = true;
            work.extend(node.parents.iter().copied());
            self.queue_target(id);
        }
        self.dirty_work = work;
    }
    pub(super) fn new_scope(&mut self) -> Result<ScopeId> {
        let slot = self.free_scopes.pop().unwrap_or(self.scopes.len());
        if slot == self.scopes.len() {
            self.scopes.push(ScopeSlot {
                generation: 0,
                nodes: None,
            });
        }
        let s = self.scopes.get_mut(slot).ok_or(Error::InvalidScope)?;
        s.nodes = Some(Vec::new());
        self.stats.live_scopes = self.stats.live_scopes.saturating_add(1);
        Ok(ScopeId {
            slot,
            generation: s.generation,
        })
    }
    pub(super) fn invalidate_scope(&mut self, scope: ScopeId) {
        let mut pending_scopes = vec![scope];
        while let Some(scope) = pending_scopes.pop() {
            let Some(slot) = self
                .scopes
                .get_mut(scope.slot)
                .filter(|s| s.generation == scope.generation)
            else {
                continue;
            };
            let Some(nodes) = slot.nodes.take() else {
                continue;
            };
            if let Some(next) = slot.generation.checked_add(1) {
                slot.generation = next;
                self.free_scopes.push(scope.slot);
            }
            self.stats.live_scopes = self.stats.live_scopes.saturating_sub(1);
            let mut work = nodes;
            while let Some(id) = work.pop() {
                let Ok(node) = self.node_mut(id) else {
                    continue;
                };
                if !node.valid {
                    continue;
                }
                node.valid = false;
                node.dirty = false;
                node.visiting = false;
                let active = node.demand > 0;
                let parents = node.parents.clone();
                let child_scope = node.body.as_ref().and_then(|b| b.scope());
                let body = node.body.take();
                let inputs = std::mem::take(&mut node.inputs);
                let watchers = std::mem::take(&mut node.observers);
                node.effects.clear();
                node.demand = 0;
                for watch in watchers {
                    self.observations.remove(&watch);
                }
                for input in inputs {
                    if active {
                        self.deactivate(input.id);
                    }
                    if let Ok(parent) = self.node_mut(input.id) {
                        parent.parents.retain(|p| *p != id);
                    }
                }
                drop(body);
                work.extend(parents);
                if let Some(child) = child_scope {
                    pending_scopes.push(child);
                }
            }
        }
    }
    pub(super) fn detach_selected(&mut self, id: Id) -> Result<()> {
        let node = self.node_mut(id)?;
        let active = node.demand > 0;
        let old = if node.inputs.len() > 1 {
            node.inputs.pop()
        } else {
            None
        };
        if let Some(old) = old {
            if active {
                self.deactivate(old.id);
            }
            let parents = &mut self.node_mut(old.id)?.parents;
            if let Some(position) = parents.iter().position(|parent| *parent == id) {
                parents.swap_remove(position);
            }
        }
        Ok(())
    }
    pub(super) fn attach_selected(&mut self, id: Id, selected: &Arc<Lease>) -> Result<()> {
        // The demand walk stops at already-active nodes. The explicit evaluation
        // stack then detects a back-edge before any observer is published; avoid
        // rescanning the entire ancestor graph for every selected sink branch.
        let node = self.node_mut(id)?;
        node.inputs.push(selected.clone());
        let active = node.demand > 0;
        self.node_mut(selected.id)?.parents.push(id);
        if active {
            self.activate(selected.id);
        }
        Ok(())
    }
    /// Release dropped handles and observations without starting an evaluation.
    /// # Errors
    /// Returns `Busy` during evaluation or `Poisoned` after a failed evaluation.
    pub fn collect(&mut self) -> Result<()> {
        self.idle()?;
        self.collect_retired();
        Ok(())
    }
    fn collect_retired(&mut self) {
        loop {
            let retired = self.retired.lock().pop();
            match retired {
                None => break,
                Some(Retired::Watch(watch)) => {
                    if let Some(obs) = self.observations.remove(&watch) {
                        let id = obs.node.id;
                        if let Ok(n) = self.node_mut(id) {
                            n.observers.retain(|w| *w != watch);
                            n.effects.retain(|w| *w != watch);
                        }
                        self.deactivate(id);
                    }
                }
                Some(Retired::Node(id)) => {
                    if self.node(id).is_ok_and(|n| n.lease.strong_count() != 0) {
                        continue;
                    }
                    let Some(slot) = self
                        .slots
                        .get_mut(id.slot)
                        .filter(|s| s.generation == id.generation)
                    else {
                        continue;
                    };
                    let Some(node) = slot.node.take() else {
                        continue;
                    };
                    if let Some(next) = slot.generation.checked_add(1) {
                        slot.generation = next;
                        self.free.push(id.slot);
                    }
                    self.stats.live_nodes = self.stats.live_nodes.saturating_sub(1);
                    self.stats.reclaimed_nodes = self.stats.reclaimed_nodes.saturating_add(1);
                    for input in &node.inputs {
                        if let Ok(parent) = self.node_mut(input.id) {
                            parent.parents.retain(|p| *p != id);
                        }
                    }
                    let scope = node.body.as_ref().and_then(|b| b.scope());
                    drop(node);
                    if let Some(scope) = scope {
                        self.invalidate_scope(scope);
                    }
                }
            }
        }
    }
    fn settle(&mut self, root: Id) -> Result<()> {
        let mut work = std::mem::take(&mut self.work);
        work.push(Work::Visit(root));
        while let Some(item) = work.pop() {
            match item {
                Work::Visit(id) => {
                    let node = self.node_mut(id)?;
                    if !node.valid || !node.dirty {
                        continue;
                    }
                    if node.visiting {
                        return Err(Error::Cycle);
                    }
                    node.visiting = true;
                    if node.switch {
                        let outer = node.inputs.first().ok_or(Error::Unsettled)?.id;
                        work.push(Work::Select(id));
                        work.push(Work::Visit(outer));
                    } else {
                        work.push(Work::Finish(id));
                        work.extend(node.inputs.iter().rev().map(|input| Work::Visit(input.id)));
                    }
                }
                Work::Select(id) => {
                    if !self.node(id)?.valid {
                        continue;
                    }
                    let mut body = self.node_mut(id)?.body.take().ok_or(Error::Cycle)?;
                    let result = body.prepare(self, id);
                    self.node_mut(id)?.body = Some(body);
                    result?;
                    let node = self.node(id)?;
                    let inner = node.inputs.get(1).ok_or(Error::Unsettled)?.id;
                    work.push(Work::Finish(id));
                    work.push(Work::Visit(inner));
                }
                Work::Finish(id) => {
                    let node = self.node(id)?;
                    if !node.valid {
                        continue;
                    }
                    let changed = node.body.as_ref().is_none_or(|b| b.value().is_none())
                        || node.inputs.iter().any(|input| {
                            self.node(input.id).is_ok_and(|n| n.changed > node.computed)
                        });
                    let mut body = self.node_mut(id)?.body.take().ok_or(Error::Cycle)?;
                    let evaluated = if changed {
                        self.stats.evaluations = self.stats.evaluations.saturating_add(1);
                        body.evaluate(self)
                    } else {
                        Ok(false)
                    };
                    self.node_mut(id)?.body = Some(body);
                    let did_change = evaluated?;
                    if !did_change {
                        self.stats.equality_cutoffs = self.stats.equality_cutoffs.saturating_add(1);
                    }
                    let epoch = self.epoch;
                    let node = self.node_mut(id)?;
                    node.dirty = false;
                    node.visiting = false;
                    node.computed = epoch;
                    if did_change {
                        node.changed = epoch;
                    }
                }
            }
        }
        self.work = work;
        Ok(())
    }
    fn publish(&mut self, targets: &[Id]) -> Result<()> {
        self.phase = Phase::Publishing;
        let mut next = Transaction {
            owner: self.owner.clone(),
            writes: Vec::new(),
        };
        for id in targets {
            let Ok(node) = self.node(*id) else { continue };
            if !node.valid {
                continue;
            }
            if node.effects.is_empty() {
                continue;
            }
            let changed = node.changed;
            let watchers = node.effects.clone();
            for watch in watchers {
                let Some(mut observation) = self.observations.remove(&watch) else {
                    continue;
                };
                if observation.last.is_none_or(|last| changed > last) {
                    if let Some(f) = &mut observation.callback {
                        let value = self
                            .node(*id)?
                            .body
                            .as_ref()
                            .and_then(|b| b.value())
                            .ok_or(Error::Unsettled)?;
                        f(value, &mut next)?;
                        self.stats.notifications = self.stats.notifications.saturating_add(1);
                    }
                    observation.last = Some(changed);
                }
                self.observations.insert(watch, observation);
            }
        }
        self.feedback.extend(next.writes);
        self.phase = Phase::Computing;
        Ok(())
    }
    /// Apply a finite input batch. A closure error aborts its staged writes.
    /// Evaluation errors poison the runtime; a panic is resumed after poisoning.
    /// # Errors
    /// Closure errors abort staging. Invalid handles fail admission. Evaluation or
    /// subscription errors poison the runtime and are returned to the caller.
    pub fn transaction(&mut self, f: impl FnOnce(&mut Transaction) -> Result<()>) -> Result<Stats> {
        self.idle()?;
        self.collect_retired();
        let mut tx = Transaction {
            owner: self.owner.clone(),
            writes: Vec::new(),
        };
        f(&mut tx)?;
        for write in &tx.writes {
            let node = self.node(write.input.id)?;
            if !node.valid {
                return Err(Error::InvalidScope);
            }
        }
        let mut writes = std::mem::take(&mut self.feedback);
        writes.extend(tx.writes);
        self.phase = Phase::Computing;
        let outcome = catch_unwind(AssertUnwindSafe(|| self.run(writes)));
        match outcome {
            Ok(Ok(())) => {
                self.phase = Phase::Idle;
                self.collect_retired();
                Ok(self.stats)
            }
            Ok(Err(e)) => {
                self.phase = Phase::Poisoned;
                Err(e)
            }
            Err(panic) => {
                self.phase = Phase::Poisoned;
                resume_unwind(panic)
            }
        }
    }
    /// Settle initial observations or process feedback from the previous transaction.
    /// # Errors
    /// Returns the same admission and evaluation errors as `transaction`.
    pub fn stabilize(&mut self) -> Result<Stats> {
        self.transaction(|_| Ok(()))
    }
    fn run(&mut self, writes: Vec<Write>) -> Result<()> {
        // Group once: repeated values must not repeatedly scan the remaining batch.
        let mut indexes = FxHashMap::<Id, usize>::default();
        let mut pending = Vec::<(Arc<Lease>, VecDeque<super::ErasedValue>)>::new();
        for write in writes {
            let index = *indexes.entry(write.input.id).or_insert_with(|| {
                let index = pending.len();
                pending.push((write.input.clone(), VecDeque::new()));
                index
            });
            pending
                .get_mut(index)
                .ok_or(Error::InvalidHandle)?
                .1
                .push_back(write.value);
        }
        let mut first = true;
        while first || !pending.is_empty() {
            first = false;
            self.epoch = self.epoch.checked_add(1).ok_or(Error::EpochExhausted)?;
            for (input, values) in &mut pending {
                let id = input.id;
                while let Some(value) = values.pop_front() {
                    let changed = self
                        .node_mut(id)?
                        .body
                        .as_mut()
                        .ok_or(Error::Unsettled)?
                        .set(value)?;
                    if changed {
                        let epoch = self.epoch;
                        self.node_mut(id)?.changed = epoch;
                        let parents = self.node(id)?.parents.clone();
                        for parent in parents {
                            self.dirty(parent);
                        }
                        self.queue_target(id);
                        break;
                    }
                    self.stats.equality_cutoffs = self.stats.equality_cutoffs.saturating_add(1);
                }
            }
            pending.retain(|(_, values)| !values.is_empty());
            let mut targets = std::mem::take(&mut self.targets);
            for id in &targets {
                if self.node(*id).is_ok_and(|n| n.valid) {
                    self.settle(*id)?;
                }
            }
            self.publish(&targets)?;
            for id in &targets {
                if let Ok(node) = self.node_mut(*id) {
                    node.queued = false;
                }
            }
            targets.clear();
            if self.targets.is_empty() {
                self.targets = targets;
            }
            self.stats.evaluation_steps = self.stats.evaluation_steps.saturating_add(1);
            self.collect_retired();
        }
        Ok(())
    }
}
