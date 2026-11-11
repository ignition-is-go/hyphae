//! Captured-shape, generic computation benchmark. Never contacts a live service.
#![allow(
    clippy::too_many_lines,
    reason = "benchmark adapters keep construction and execution together"
)]

use std::{
    collections::VecDeque,
    env,
    hint::black_box,
    sync::{Arc, mpsc},
    thread,
    time::{Duration, Instant},
};

use hyphae::{
    Cell, CellImmutable, CellMutable, Gettable, MapExt, Materialize, Mutable, SwitchMapExt, batch,
    join_vec,
};
use incremental::{Incr, IncrState, Observer, Var};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Clone)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent benchmark dimensions, not a state machine"
)]
struct Config {
    backend: String,
    scenes: usize,
    clients: usize,
    layers: usize,
    branch_depth: usize,
    switch_every: u64,
    rebuild: bool,
    frames: u64,
    hz: u64,
    queue: usize,
    encode_each: bool,
    verify_every: u64,
    staggered: bool,
    dynamic_sinks: bool,
    expand_fields: bool,
}

impl Config {
    fn parse() -> Result<Self, String> {
        let mut c = Self {
            backend: "hyphae".into(),
            scenes: 28,
            clients: 12,
            layers: 4,
            branch_depth: 4,
            switch_every: 60,
            rebuild: true,
            frames: 180,
            hz: 60,
            queue: 64,
            encode_each: false,
            verify_every: 1,
            staggered: true,
            dynamic_sinks: false,
            expand_fields: true,
        };
        let mut args = env::args().skip(1);
        while let Some(key) = args.next() {
            if key == "--bench" {
                continue;
            }
            if key == "--help" {
                println!(
                    "scene_workload --backend hyphae|incremental|local --scenes 10|20|28 --clients N --layers N --branch-depth N --switch-every N --switch-mode rebuild|select --frames N --hz N --queue N --encode shared|per-client --verify-every N --feedback staggered|batched --dynamic-sinks on|off --fields authored|one; hz=0 means saturation"
                );
                return Err("help".into());
            }
            let value = args
                .next()
                .ok_or_else(|| format!("missing value for {key}"))?;
            let number = || {
                value
                    .parse::<u64>()
                    .map_err(|_| format!("bad integer for {key}"))
            };
            let size = || usize::try_from(number()?).map_err(|_| format!("{key} too large"));
            match key.as_str() {
                "--backend" => c.backend = value,
                "--feedback" => match value.as_str() {
                    "staggered" => c.staggered = true,
                    "batched" => c.staggered = false,
                    _ => return Err("invalid feedback mode".into()),
                },
                "--dynamic-sinks" => match value.as_str() {
                    "on" => c.dynamic_sinks = true,
                    "off" => c.dynamic_sinks = false,
                    _ => return Err("invalid dynamic sink mode".into()),
                },
                "--fields" => match value.as_str() {
                    "authored" => c.expand_fields = true,
                    "one" => c.expand_fields = false,
                    _ => return Err("invalid field expansion".into()),
                },
                "--scenes" => c.scenes = size()?,
                "--clients" => c.clients = size()?,
                "--layers" => c.layers = size()?,
                "--branch-depth" => c.branch_depth = size()?,
                "--switch-every" => c.switch_every = number()?,
                "--frames" => c.frames = number()?,
                "--hz" => c.hz = number()?,
                "--queue" => c.queue = size()?,
                "--verify-every" => c.verify_every = number()?,
                "--switch-mode" => match value.as_str() {
                    "rebuild" => c.rebuild = true,
                    "select" => c.rebuild = false,
                    _ => return Err("invalid switch mode".into()),
                },
                "--encode" => match value.as_str() {
                    "shared" => c.encode_each = false,
                    "per-client" => c.encode_each = true,
                    _ => return Err("invalid encoding mode".into()),
                },
                _ => return Err(format!("unknown option {key}")),
            }
        }
        if !matches!(c.backend.as_str(), "hyphae" | "incremental" | "local")
            || c.scenes == 0
            || c.scenes > 28
            || c.clients == 0
            || c.layers == 0
            || c.frames == 0
            || c.queue == 0
            || c.verify_every == 0
        {
            return Err("invalid backend or dimensions".into());
        }
        Ok(c)
    }
}

#[derive(Deserialize)]
struct FixtureNode {
    scene: usize,
    kind: String,
    role: String,
    authored_fields: usize,
}
#[derive(Deserialize)]
struct Group {
    kind: String,
    nodes: Vec<usize>,
}
#[derive(Deserialize)]
struct Fixture {
    nodes: Vec<FixtureNode>,
    edges: Vec<[usize; 2]>,
    scene_output_links: Vec<(Option<usize>, usize)>,
    shared_groups: Vec<Group>,
}
#[derive(Clone)]
struct Node {
    inputs: Vec<usize>,
    scene: usize,
    select: bool,
    dynamic: bool,
    salt: u64,
    layers: usize,
}
struct Plan {
    nodes: Vec<Node>,
    outputs: Vec<usize>,
    clock_roots: Vec<usize>,
    feedback_roots: Vec<usize>,
    control_roots: Vec<usize>,
    roots: usize,
    property_fields: usize,
    comp_fields: usize,
    source_nodes: usize,
    source_edges: usize,
    missing_publishers: usize,
    omitted_publishers: usize,
}

fn u64_of(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}
fn slot(n: u64, len: usize) -> usize {
    usize::try_from(n.checked_rem(u64_of(len)).unwrap_or(0)).unwrap_or(0)
}
const fn mix(value: u64, salt: u64) -> u64 {
    value.rotate_left(7).wrapping_mul(31).wrapping_add(salt)
}
fn transform(mut value: u64, salt: u64, layers: usize) -> u64 {
    for layer in 0..layers {
        value = mix(value, salt.wrapping_add(u64_of(layer)));
    }
    value
}
fn reduce(values: &[u64]) -> u64 {
    values.iter().copied().fold(0_u64, u64::wrapping_add)
}

impl Plan {
    fn build(cfg: &Config) -> Result<Self, String> {
        let f: Fixture = serde_json::from_str(include_str!("fixtures/scene_topology.json"))
            .map_err(|e| e.to_string())?;
        let mut sizes = vec![0_usize; 28];
        for node in &f.nodes {
            if let Some(n) = sizes.get_mut(node.scene) {
                *n = n.saturating_add(1);
            }
        }
        let mut scenes: Vec<_> = (0..28).collect();
        scenes.sort_by_key(|s| std::cmp::Reverse(sizes.get(*s).copied().unwrap_or(0)));
        scenes.truncate(cfg.scenes);
        let included = |i: usize| {
            f.nodes
                .get(i)
                .is_some_and(|n| scenes.contains(&n.scene) && n.role != "annotation")
        };
        let mut deps = vec![Vec::new(); f.nodes.len()];
        for [from, to] in &f.edges {
            if included(*from) && included(*to) {
                deps.get_mut(*to).ok_or("invalid edge")?.push(*from);
            }
        }
        let mut missing_publishers = 0_usize;
        let mut omitted_publishers = 0_usize;
        for (from, to) in &f.scene_output_links {
            if !included(*to) {
                continue;
            }
            if let Some(from) = from.filter(|from| included(*from)) {
                deps.get_mut(*to).ok_or("invalid reference")?.push(from);
            } else if from.is_none() {
                missing_publishers = missing_publishers.saturating_add(1);
            } else {
                omitted_publishers = omitted_publishers.saturating_add(1);
            }
        }
        let source_edges = deps.iter().map(Vec::len).sum();
        // Build a deterministic topological order, rejecting cycles rather than silently cutting them.
        let mut degree: Vec<_> = deps.iter().map(Vec::len).collect();
        let mut children = vec![Vec::new(); f.nodes.len()];
        for (to, inputs) in deps.iter().enumerate() {
            for from in inputs {
                children.get_mut(*from).ok_or("invalid parent")?.push(to);
            }
        }
        let mut ready: VecDeque<_> = degree
            .iter()
            .enumerate()
            .filter(|(i, n)| included(*i) && **n == 0)
            .map(|(i, _)| i)
            .collect();
        let mut order = Vec::new();
        while let Some(i) = ready.pop_front() {
            order.push(i);
            for child in children.get(i).ok_or("invalid children")? {
                let d = degree.get_mut(*child).ok_or("invalid degree")?;
                *d = d.saturating_sub(1);
                if *d == 0 {
                    ready.push_back(*child);
                }
            }
        }
        let source_nodes = f
            .nodes
            .iter()
            .enumerate()
            .filter(|(i, _)| included(*i))
            .count();
        if order.len() != source_nodes {
            return Err("captured topology contains a cycle".into());
        }
        let mut node_roots = vec![Vec::new(); f.nodes.len()];
        let mut clock_roots = vec![0];
        let mut control_roots = Vec::new();
        let mut roots = 1_usize;
        for group in &f.shared_groups {
            if group.kind != "control-ref-id" || !group.nodes.iter().any(|n| included(*n)) {
                continue;
            }
            for n in &group.nodes {
                if included(*n) {
                    node_roots
                        .get_mut(*n)
                        .ok_or("invalid control reference")?
                        .push(roots);
                }
            }
            control_roots.push(roots);
            roots = roots.saturating_add(1);
        }
        let feedback_roots: Vec<_> = (0..cfg.clients)
            .map(|_| {
                let id = roots;
                roots = roots.saturating_add(1);
                id
            })
            .collect();
        for (i, node) in f.nodes.iter().enumerate() {
            if !included(i) {
                continue;
            }
            let target = node_roots.get_mut(i).ok_or("invalid root")?;
            match node.role.as_str() {
                "clock" => target.push(0),
                "input" if target.is_empty() => target.push(
                    *feedback_roots
                        .get(i.checked_rem(cfg.clients).unwrap_or(0))
                        .ok_or("missing feedback")?,
                ),
                _ => {}
            }
            if node.kind == "read-scene-output" && deps.get(i).is_some_and(Vec::is_empty) {
                target.push(0);
            }
        }
        clock_roots.sort_unstable();
        let mut mapped = vec![None; f.nodes.len()];
        let mut nodes = Vec::new();
        for original in &order {
            let n = f.nodes.get(*original).ok_or("missing node")?;
            let mut inputs = node_roots.get(*original).ok_or("missing roots")?.clone();
            for parent in deps.get(*original).ok_or("missing inputs")? {
                inputs.push(
                    mapped
                        .get(*parent)
                        .copied()
                        .flatten()
                        .ok_or("parent not ordered")?,
                );
            }
            *mapped.get_mut(*original).ok_or("missing mapping")? =
                Some(roots.saturating_add(nodes.len()));
            nodes.push(Node {
                inputs,
                scene: n.scene,
                select: n.role == "select",
                dynamic: cfg.dynamic_sinks && n.role == "sink",
                salt: u64_of(*original).wrapping_add(1),
                layers: cfg.layers,
            });
        }
        let mut outputs = Vec::new();
        let mut property_fields = 0_usize;
        let mut comp_fields = 0_usize;
        for original in &order {
            let n = f.nodes.get(*original).ok_or("missing output")?;
            if n.role != "sink" {
                continue;
            }
            let parent = mapped
                .get(*original)
                .copied()
                .flatten()
                .ok_or("missing output mapping")?;
            // Authored fields are a scale proxy, not a count of live changing caps.
            let fields = if n.kind == "comp-element" && cfg.expand_fields {
                n.authored_fields.saturating_sub(2).max(1)
            } else {
                1
            };
            if n.kind == "comp-element" {
                comp_fields = comp_fields.saturating_add(fields);
            } else {
                property_fields = property_fields.saturating_add(fields);
            }
            for field in 0..fields {
                outputs.push(roots.saturating_add(nodes.len()));
                nodes.push(Node {
                    inputs: vec![parent],
                    scene: n.scene,
                    select: false,
                    dynamic: false,
                    salt: u64_of(field).wrapping_add(10_000),
                    layers: 1,
                });
            }
        }
        Ok(Self {
            nodes,
            outputs,
            clock_roots,
            feedback_roots,
            control_roots,
            roots,
            property_fields,
            comp_fields,
            source_nodes,
            source_edges,
            missing_publishers,
            omitted_publishers,
        })
    }

    fn update(&self, sequence: u64, values: &mut [u64], cfg: &Config) -> Vec<(usize, u64)> {
        let mut changes = Vec::new();
        let phase = if cfg.staggered {
            slot(sequence.saturating_sub(1), cfg.clients.saturating_add(1))
        } else {
            0
        };
        let frame = if cfg.staggered {
            sequence
                .saturating_sub(1)
                .checked_div(u64_of(cfg.clients.saturating_add(1)))
                .unwrap_or(0)
                .saturating_add(1)
        } else {
            sequence
        };
        if phase == 0 {
            for id in &self.clock_roots {
                changes.push((*id, frame));
            }
            if let Some(id) = self
                .control_roots
                .get(slot(frame, self.control_roots.len()))
            {
                changes.push((*id, frame));
            }
        }
        for (client, id) in self.feedback_roots.iter().enumerate() {
            if !cfg.staggered || phase == client.saturating_add(1) {
                changes.push((*id, frame.wrapping_add(u64_of(client))));
            }
        }
        for (id, value) in &changes {
            if let Some(v) = values.get_mut(*id) {
                *v = *value;
            }
        }
        changes
    }

    fn reference(
        &self,
        roots: &[u64],
        revisions: &[u64],
        cfg: &Config,
    ) -> Result<Vec<u64>, String> {
        let mut values = roots.to_vec();
        for node in &self.nodes {
            let inputs: Vec<_> = node
                .inputs
                .iter()
                .map(|id| values.get(*id).copied().ok_or("reference input missing"))
                .collect::<Result<_, _>>()?;
            let revision = *revisions.get(node.scene).ok_or("reference scene missing")?;
            let value = if node.select && !inputs.is_empty() {
                let selected = *inputs
                    .get(slot(revision, inputs.len()))
                    .ok_or("reference branch missing")?;
                if cfg.rebuild {
                    transform(selected, revision, cfg.branch_depth)
                } else {
                    selected
                }
            } else {
                reduce(&inputs)
            };
            values.push(transform(value, node.salt, node.layers));
        }
        self.outputs
            .iter()
            .map(|id| {
                values
                    .get(*id)
                    .copied()
                    .ok_or_else(|| "reference output missing".into())
            })
            .collect()
    }
}

trait Engine {
    fn metrics(&self) -> Option<Value> {
        None
    }
    fn evaluate(
        &mut self,
        changes: &[(usize, u64)],
        revisions: &[(usize, u64)],
    ) -> Result<Vec<u64>, String>;
}

struct HyphaeEngine {
    roots: Vec<Cell<u64, CellMutable>>,
    selectors: Vec<Cell<u64, CellMutable>>,
    outputs: Vec<Cell<u64, CellImmutable>>,
}
impl HyphaeEngine {
    fn build(plan: &Plan, cfg: &Config) -> Result<Self, String> {
        let roots: Vec<_> = (0..plan.roots).map(|_| Cell::new(0_u64)).collect();
        let selectors: Vec<_> = (0..28).map(|_| Cell::new(0_u64)).collect();
        let mut cells: Vec<_> = roots.iter().cloned().map(Cell::lock).collect();
        for node in &plan.nodes {
            let inputs: Vec<_> = node
                .inputs
                .iter()
                .map(|i| cells.get(*i).cloned().ok_or("missing Hyphae input"))
                .collect::<Result<_, _>>()?;
            let base = if node.select && !inputs.is_empty() {
                let selector = selectors
                    .get(node.scene)
                    .ok_or("missing Hyphae selector")?
                    .clone();
                let rebuild = cfg.rebuild;
                let depth = cfg.branch_depth;
                selector
                    .switch_map(move |revision| {
                        let input = inputs
                            .get(slot(*revision, inputs.len()))
                            .cloned()
                            .unwrap_or_else(|| Cell::new(0).lock());
                        if rebuild {
                            let revision = *revision;
                            let mut tail = input;
                            for layer in 0..depth {
                                tail = tail
                                    .map(move |value| {
                                        mix(*value, revision.wrapping_add(u64_of(layer)))
                                    })
                                    .materialize();
                            }
                            tail
                        } else {
                            input
                        }
                    })
                    .materialize()
            } else if inputs.is_empty() {
                Cell::new(0_u64).lock()
            } else {
                join_vec(inputs).map(|values| reduce(values)).materialize()
            };
            let base = if node.dynamic {
                let selector = selectors
                    .get(node.scene)
                    .ok_or("missing dynamic selector")?
                    .clone();
                selector
                    .switch_map(move |_| base.clone().map(|v| *v).materialize())
                    .materialize()
            } else {
                base
            };
            let mut tail = base;
            for layer in 0..node.layers {
                let salt = node.salt.wrapping_add(u64_of(layer));
                tail = tail.map(move |value| mix(*value, salt)).materialize();
            }
            cells.push(tail);
        }
        let outputs = plan
            .outputs
            .iter()
            .map(|i| cells.get(*i).cloned().ok_or("missing Hyphae output"))
            .collect::<Result<_, _>>()?;
        Ok(Self {
            roots,
            selectors,
            outputs,
        })
    }
}
impl Engine for HyphaeEngine {
    fn evaluate(
        &mut self,
        changes: &[(usize, u64)],
        revisions: &[(usize, u64)],
    ) -> Result<Vec<u64>, String> {
        batch(|| {
            for (i, v) in changes {
                if let Some(cell) = self.roots.get(*i) {
                    cell.set(*v);
                }
            }
            for (i, v) in revisions {
                if let Some(cell) = self.selectors.get(*i) {
                    cell.set(*v);
                }
            }
        });
        Ok(self.outputs.iter().map(Gettable::get).collect())
    }
}

struct IncrementalEngine {
    state: IncrState,
    roots: Vec<Var<u64>>,
    selectors: Vec<Var<u64>>,
    outputs: Vec<Observer<u64>>,
}
impl IncrementalEngine {
    fn build(plan: &Plan, cfg: &Config) -> Result<Self, String> {
        let state = IncrState::new();
        state.set_max_height_allowed(100_000);
        let roots: Vec<_> = (0..plan.roots).map(|_| state.var(0_u64)).collect();
        let selectors: Vec<_> = (0..28).map(|_| state.var(0_u64)).collect();
        let mut cells: Vec<Incr<u64>> = roots.iter().map(Var::watch).collect();
        for node in &plan.nodes {
            let inputs: Vec<_> = node
                .inputs
                .iter()
                .map(|i| cells.get(*i).cloned().ok_or("missing Incremental input"))
                .collect::<Result<_, _>>()?;
            let base = if node.select && !inputs.is_empty() {
                let selector = selectors
                    .get(node.scene)
                    .ok_or("missing Incremental selector")?
                    .watch();
                let rebuild = cfg.rebuild;
                let depth = cfg.branch_depth;
                let fallback = inputs
                    .first()
                    .cloned()
                    .ok_or("empty Incremental branch list")?;
                selector.bind(move |revision| {
                    // Plan validation guarantees a nonempty input list and an in-range modulo.
                    let mut tail = inputs
                        .get(slot(*revision, inputs.len()))
                        .cloned()
                        .unwrap_or_else(|| fallback.clone());
                    if rebuild {
                        for layer in 0..depth {
                            let salt = revision.wrapping_add(u64_of(layer));
                            tail = tail.map(move |v| mix(*v, salt));
                        }
                    }
                    tail
                })
            } else {
                state.fold(inputs, 0_u64, |sum, v| sum.wrapping_add(*v))
            };
            let base = if node.dynamic {
                selectors
                    .get(node.scene)
                    .ok_or("missing dynamic selector")?
                    .watch()
                    .bind(move |_| base.map(|v| *v))
            } else {
                base
            };
            let mut tail = base;
            for layer in 0..node.layers {
                let salt = node.salt.wrapping_add(u64_of(layer));
                tail = tail.map(move |v| mix(*v, salt));
            }
            cells.push(tail);
        }
        let outputs = plan
            .outputs
            .iter()
            .map(|i| {
                cells
                    .get(*i)
                    .map(Incr::observe)
                    .ok_or("missing Incremental output")
            })
            .collect::<Result<_, _>>()?;
        state.stabilise();
        Ok(Self {
            state,
            roots,
            selectors,
            outputs,
        })
    }
}
impl Engine for IncrementalEngine {
    fn evaluate(
        &mut self,
        changes: &[(usize, u64)],
        revisions: &[(usize, u64)],
    ) -> Result<Vec<u64>, String> {
        for (i, v) in changes {
            self.roots
                .get(*i)
                .ok_or("missing Incremental root")?
                .set(*v);
        }
        for (i, v) in revisions {
            self.selectors
                .get(*i)
                .ok_or("missing Incremental revision")?
                .set(*v);
        }
        self.state.stabilise();
        self.outputs
            .iter()
            .map(|o| o.try_get_value().map_err(|e| format!("{e:?}")))
            .collect()
    }
}

#[cfg(feature = "local-runtime")]
struct LocalEngine {
    rt: hyphae::local::Runtime,
    roots: Vec<hyphae::local::Input<u64>>,
    selectors: Vec<hyphae::local::Input<u64>>,
    outputs: Vec<hyphae::local::Observer<u64>>,
}
#[cfg(feature = "local-runtime")]
impl LocalEngine {
    fn build(plan: &Plan, cfg: &Config) -> Result<Self, String> {
        let build = || -> Result<Self, hyphae::local::Error> {
            let mut rt = hyphae::local::Runtime::new();
            let roots: Vec<_> = (0..plan.roots)
                .map(|_| rt.input(0_u64))
                .collect::<Result<_, _>>()?;
            let selectors: Vec<_> = (0..28).map(|_| rt.input(0_u64)).collect::<Result<_, _>>()?;
            let mut cells: Vec<_> = roots.iter().map(hyphae::local::Input::cell).collect();
            for node in &plan.nodes {
                let inputs: Vec<_> = node
                    .inputs
                    .iter()
                    .map(|i| {
                        cells
                            .get(*i)
                            .cloned()
                            .ok_or(hyphae::local::Error::InvalidHandle)
                    })
                    .collect::<Result<_, _>>()?;
                let base = if node.select && !inputs.is_empty() {
                    let selector = selectors
                        .get(node.scene)
                        .ok_or(hyphae::local::Error::InvalidHandle)?
                        .cell();
                    let rebuild = cfg.rebuild;
                    let depth = cfg.branch_depth;
                    rt.switch_map(&selector, move |b, revision| {
                        let mut tail = inputs
                            .get(slot(*revision, inputs.len()))
                            .cloned()
                            .ok_or(hyphae::local::Error::InvalidHandle)?;
                        if rebuild {
                            for layer in 0..depth {
                                let salt = revision.wrapping_add(u64_of(layer));
                                tail = b.map(&tail, move |v| mix(*v, salt))?;
                            }
                        }
                        Ok(tail)
                    })?
                } else {
                    rt.fold(&inputs, 0_u64, |sum, v| sum.wrapping_add(*v))?
                };
                let base = if node.dynamic {
                    let selector = selectors
                        .get(node.scene)
                        .ok_or(hyphae::local::Error::InvalidHandle)?
                        .cell();
                    rt.switch_map(&selector, move |b, _| b.map(&base, |v| *v))?
                } else {
                    base
                };
                let mut tail = base;
                for layer in 0..node.layers {
                    let salt = node.salt.wrapping_add(u64_of(layer));
                    tail = rt.map(&tail, move |v| mix(*v, salt))?;
                }
                cells.push(tail);
            }
            let outputs = plan
                .outputs
                .iter()
                .map(|i| rt.observe(cells.get(*i).ok_or(hyphae::local::Error::InvalidHandle)?))
                .collect::<Result<_, _>>()?;
            rt.stabilize()?;
            Ok(Self {
                rt,
                roots,
                selectors,
                outputs,
            })
        };
        build().map_err(|e| e.to_string())
    }
}
#[cfg(feature = "local-runtime")]
impl Engine for LocalEngine {
    fn metrics(&self) -> Option<Value> {
        let s = self.rt.stats();
        Some(
            json!({"live_nodes":s.live_nodes,"live_scopes":s.live_scopes,"allocated_nodes":s.allocated_nodes,"reclaimed_nodes":s.reclaimed_nodes,"evaluations":s.evaluations,"equality_cutoffs":s.equality_cutoffs,"evaluation_steps":s.evaluation_steps}),
        )
    }
    fn evaluate(
        &mut self,
        changes: &[(usize, u64)],
        revisions: &[(usize, u64)],
    ) -> Result<Vec<u64>, String> {
        self.rt
            .transaction(|tx| {
                for (i, v) in changes {
                    tx.set(
                        self.roots
                            .get(*i)
                            .ok_or(hyphae::local::Error::InvalidHandle)?,
                        *v,
                    )?;
                }
                for (i, v) in revisions {
                    tx.set(
                        self.selectors
                            .get(*i)
                            .ok_or(hyphae::local::Error::InvalidHandle)?,
                        *v,
                    )?;
                }
                Ok(())
            })
            .map_err(|e| e.to_string())?;
        self.outputs
            .iter()
            .map(|o| o.get(&self.rt).map_err(|e| e.to_string()))
            .collect()
    }
}

struct Frame {
    sequence: u64,
    scheduled: Instant,
    enqueued: Instant,
}
struct Measurements {
    eval: Vec<u128>,
    encoding: Vec<u128>,
    latency: Vec<u128>,
    verification: Vec<u128>,
    switches: Vec<u128>,
    runtime_metrics: Option<Value>,
    producer: Vec<u128>,
    queue_wait: Vec<u128>,
    setup_ns: u128,
    teardown_ns: u128,
    bytes: u64,
    delivered: u64,
    checked: u64,
    missed: u64,
}
fn percentiles(mut samples: Vec<u128>) -> Value {
    samples.sort_unstable();
    let at = |p: usize| {
        samples
            .get(
                samples
                    .len()
                    .saturating_sub(1)
                    .saturating_mul(p)
                    .checked_div(100)
                    .unwrap_or(0),
            )
            .copied()
    };
    json!({"count":samples.len(),"p50":at(50),"p95":at(95),"p99":at(99),"max":samples.last()})
}
fn worker(
    plan: &Plan,
    cfg: &Config,
    rx: mpsc::Receiver<Frame>,
    ready: &mpsc::Sender<()>,
) -> Result<Measurements, String> {
    let setup = Instant::now();
    let mut engine: Box<dyn Engine> = match cfg.backend.as_str() {
        "hyphae" => Box::new(HyphaeEngine::build(plan, cfg)?),
        "incremental" => Box::new(IncrementalEngine::build(plan, cfg)?),
        #[cfg(feature = "local-runtime")]
        "local" => Box::new(LocalEngine::build(plan, cfg)?),
        _ => return Err("backend requires the local-runtime feature".into()),
    };
    let setup_ns = setup.elapsed().as_nanos();
    let mut roots = vec![0_u64; plan.roots];
    let mut revisions = vec![0_u64; 28];
    let initial = engine.evaluate(&[], &[])?;
    if initial != plan.reference(&roots, &revisions, cfg)? {
        return Err("initial oracle mismatch".into());
    }
    ready.send(()).map_err(|e| e.to_string())?;
    let mut m = Measurements {
        eval: Vec::new(),
        encoding: Vec::new(),
        latency: Vec::new(),
        verification: Vec::new(),
        switches: Vec::new(),
        runtime_metrics: None,
        producer: Vec::new(),
        queue_wait: Vec::new(),
        setup_ns,
        teardown_ns: 0,
        bytes: 0,
        delivered: 0,
        checked: 0,
        missed: 0,
    };
    let mut previous = initial;
    for frame in rx {
        m.producer.push(
            frame
                .enqueued
                .saturating_duration_since(frame.scheduled)
                .as_nanos(),
        );
        m.queue_wait.push(frame.enqueued.elapsed().as_nanos());
        let changes = plan.update(frame.sequence, &mut roots, cfg);
        let phases = if cfg.staggered {
            u64_of(cfg.clients.saturating_add(1))
        } else {
            1
        };
        let logical_frame = frame
            .sequence
            .saturating_sub(1)
            .checked_div(phases)
            .unwrap_or(0)
            .saturating_add(1);
        let switching = cfg.switch_every != 0
            && logical_frame.is_multiple_of(cfg.switch_every)
            && frame.sequence.saturating_sub(1).is_multiple_of(phases);
        let mut revision_changes = Vec::new();
        if switching {
            for (scene, value) in revisions.iter_mut().enumerate() {
                *value = value.wrapping_add(1);
                revision_changes.push((scene, *value));
            }
        }
        let start = Instant::now();
        let output = engine.evaluate(&changes, &revision_changes)?;
        let eval_ns = start.elapsed().as_nanos();
        m.eval.push(eval_ns);
        if switching {
            m.switches.push(eval_ns);
        }
        let encode = Instant::now();
        let delta: Vec<_> = output
            .iter()
            .zip(&previous)
            .enumerate()
            .filter(|(_, (new, old))| new != old)
            .map(|(i, (v, _))| (i, *v))
            .collect();
        if cfg.encode_each {
            for client in 0..cfg.clients {
                let bytes = serde_json::to_vec(&(frame.sequence, client, &delta))
                    .map_err(|e| e.to_string())?;
                m.bytes = m.bytes.saturating_add(u64_of(bytes.len()));
                black_box(bytes);
            }
        } else {
            let bytes =
                Arc::new(serde_json::to_vec(&(frame.sequence, &delta)).map_err(|e| e.to_string())?);
            for _ in 0..cfg.clients {
                m.bytes = m.bytes.saturating_add(u64_of(bytes.len()));
                black_box(Arc::clone(&bytes));
            }
        }
        m.encoding.push(encode.elapsed().as_nanos());
        let latency = frame.scheduled.elapsed();
        m.latency.push(latency.as_nanos());
        if cfg.hz != 0
            && latency
                > Duration::from_secs_f64(
                    1.0 / f64::from(u32::try_from(cfg.hz).unwrap_or(u32::MAX)),
                )
        {
            m.missed = m.missed.saturating_add(1);
        }
        m.delivered = m.delivered.saturating_add(u64_of(cfg.clients));
        previous.clone_from(&output);
        if frame.sequence.is_multiple_of(cfg.verify_every)
            || frame.sequence
                == cfg.frames.saturating_mul(if cfg.staggered {
                    u64_of(cfg.clients.saturating_add(1))
                } else {
                    1
                })
        {
            let verify = Instant::now();
            let expected = plan.reference(&roots, &revisions, cfg)?;
            if output != expected {
                return Err(format!("oracle mismatch at frame {}", frame.sequence));
            }
            m.checked = m.checked.saturating_add(1);
            m.verification.push(verify.elapsed().as_nanos());
        }
    }
    if previous != plan.reference(&roots, &revisions, cfg)? {
        return Err("final oracle mismatch".into());
    }
    m.runtime_metrics = engine.metrics();
    let teardown = Instant::now();
    drop(engine);
    m.teardown_ns = teardown.elapsed().as_nanos();
    Ok(m)
}

async fn run(cfg: Config) -> Result<(), String> {
    let plan = Plan::build(&cfg)?;
    let summary = json!({"source_nodes":plan.source_nodes,"source_edges":plan.source_edges,"expanded_nodes":plan.nodes.len(),"roots":plan.roots,"outputs":plan.outputs.len(),"property_action_fields":plan.property_fields,"comp_field_proxy":plan.comp_fields,"external_publishers":plan.missing_publishers,"publishers_outside_selected_scenes":plan.omitted_publishers});
    let (tx, rx) = mpsc::sync_channel(cfg.queue);
    let (ready_tx, ready_rx) = mpsc::channel();
    let worker_cfg = cfg.clone();
    let handle = thread::Builder::new()
        .name("scene-evaluator".into())
        .spawn(move || worker(&plan, &worker_cfg, rx, &ready_tx))
        .map_err(|e| e.to_string())?;
    ready_rx.recv().map_err(|e| e.to_string())?;
    let start = Instant::now();
    let mut rejected = 0_u64;
    let mut admitted = 0_u64;
    let period = if cfg.hz == 0 {
        Duration::ZERO
    } else {
        Duration::from_secs_f64(1.0 / f64::from(u32::try_from(cfg.hz).unwrap_or(u32::MAX)))
    };
    let phases = if cfg.staggered {
        u64_of(cfg.clients.saturating_add(1))
    } else {
        1
    };
    let events = cfg.frames.saturating_mul(phases);
    let event_period = period
        .checked_div(u32::try_from(phases).unwrap_or(u32::MAX))
        .ok_or("invalid phases")?;
    for sequence in 1..=events {
        let offset = event_period.saturating_mul(u32::try_from(sequence).unwrap_or(u32::MAX));
        let scheduled = start.checked_add(offset).ok_or("schedule overflow")?;
        if cfg.hz != 0 {
            tokio::time::sleep_until(tokio::time::Instant::from_std(scheduled)).await;
        }
        let frame = Frame {
            sequence,
            enqueued: Instant::now(),
            scheduled: if cfg.hz == 0 {
                Instant::now()
            } else {
                scheduled
            },
        };
        if cfg.hz == 0 {
            tx.send(frame).map_err(|e| e.to_string())?;
            admitted = admitted.saturating_add(1);
        } else {
            match tx.try_send(frame) {
                Ok(()) => admitted = admitted.saturating_add(1),
                Err(mpsc::TrySendError::Full(_)) => rejected = rejected.saturating_add(1),
                Err(e) => return Err(e.to_string()),
            }
        }
    }
    drop(tx);
    let m = handle.join().map_err(|_| "evaluator panicked")??;
    let elapsed = start.elapsed().as_nanos();
    if u64_of(m.eval.len()) != admitted
        || m.delivered != admitted.saturating_mul(u64_of(cfg.clients))
    {
        return Err("frame delivery accounting mismatch".into());
    }
    println!(
        "{}",
        json!({"backend":cfg.backend,"runtime_metrics":m.runtime_metrics,"scenes":cfg.scenes,"clients":cfg.clients,"layers":cfg.layers,"branch_depth":cfg.branch_depth,"switch_every":cfg.switch_every,"switch_mode":if cfg.rebuild{"rebuild"}else{"select"},"encoding":if cfg.encode_each{"per-client"}else{"shared"},"hz":cfg.hz,"logical_frames":cfg.frames,"events_offered":events,"feedback":if cfg.staggered{"staggered"}else{"batched"},"dynamic_sinks":cfg.dynamic_sinks,"field_expansion":if cfg.expand_fields{"authored"}else{"one"},"events_admitted":admitted,"events_rejected":rejected,"events_checked":m.checked,"deadline_misses":m.missed,"queue_capacity":cfg.queue,"wall_ns":elapsed,"setup_ns":m.setup_ns,"teardown_ns":m.teardown_ns,"bytes_fanned_out":m.bytes,"simulated_deliveries":m.delivered,"plan":summary,"evaluation_ns":percentiles(m.eval),"switch_evaluation_ns":percentiles(m.switches),"encoding_ns":percentiles(m.encoding),"scheduled_to_fanout_ns":percentiles(m.latency),"oracle_ns":percentiles(m.verification),"producer_lateness_ns":percentiles(m.producer),"queue_wait_ns":percentiles(m.queue_wait),"hyphae_workers_env":env::var("HYPHAE_WORKER_THREADS").ok(),"hyphae_wave_threshold_env":env::var("HYPHAE_WAVE_THRESHOLD").ok()})
    );
    Ok(())
}
fn main() -> Result<(), String> {
    let cfg = match Config::parse() {
        Ok(c) => c,
        Err(e) if e == "help" => return Ok(()),
        Err(e) => return Err(e),
    };
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .map_err(|e| e.to_string())?
        .block_on(run(cfg))
}
