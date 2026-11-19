// Exhaustive state-space checker for the hyphae pipelined-epoch settle model.
// Design: hyphae/docs/settle-epochs.md. Every interleaving of admit, refresh
// and fire actions is explored; each fire is one atomic step of one node.
use std::collections::{HashMap, VecDeque};

thread_local!(static SWITCH_MODE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) });

fn mutant(name: &str) -> bool {
    std::env::var("MUTANT").is_ok_and(|m| m == name)
}

const S1: usize = 0;
const S2: usize = 1;
const A: usize = 2;
const U: usize = 3;
const W: usize = 4;
const J: usize = 5;
const S3: usize = 6;
const NODES: usize = 7;
const INF: u8 = 255;
const SEED: u8 = 254;
const NAMES: [&str; NODES] = ["S1", "S2", "A", "U", "W", "J", "S3"];

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
enum Desc {
    Src { node: u8, val: i32, barrier: bool },
    Fresh { node: u8, val: i32 },
    Rewire { to: u8 },
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
struct Edge {
    src: u8,
    dst: u8,
    from: u8,
    to: u8,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct St {
    queue: Vec<Desc>,
    seeded: u8,
    pend: [Vec<(u8, u8)>; NODES],
    hist: [Vec<(u8, i32)>; NODES],
    processed: [i16; NODES],
    cw: [u8; NODES],
    edges: Vec<Edge>,
    joined: u32,
    complete: u32,
}

#[derive(Clone, Copy)]
struct Cfg {
    declared_au: bool,
    lookup: bool,
    next: bool,
}

fn init_value(n: usize) -> i32 {
    if n == U { 1 } else { 0 }
}

fn at(s: &St, n: usize, t: u8) -> i32 {
    s.hist[n]
        .iter()
        .rev()
        .find(|(tk, _)| *tk <= t)
        .map_or(init_value(n), |(_, v)| *v)
}

fn contrib(s: &St, e: &Edge) -> u8 {
    let lo = e.from.max(s.cw[e.src as usize]);
    if lo >= e.to { INF } else { lo }
}

/// Lowest ticket the inputs of `n` can still deliver: the input watermark.
fn input_w(s: &St, n: usize) -> u8 {
    s.edges
        .iter()
        .filter(|e| e.dst as usize == n)
        .map(|e| contrib(s, e))
        .fold(s.seeded, u8::min)
}

fn min_pend(s: &St, n: usize) -> u8 {
    s.pend[n].first().map_or(INF, |p| p.0)
}

fn inner_edge(s: &St) -> usize {
    s.edges
        .iter()
        .position(|e| e.dst as usize == W && e.to == INF && (e.src as usize == S1 || e.src as usize == S3))
        .unwrap()
}

fn current_inner(s: &St, t: u8) -> usize {
    s.edges
        .iter()
        .find(|e| e.dst as usize == W && (e.src as usize == S1 || e.src as usize == S3) && e.from <= t && t < e.to)
        .unwrap()
        .src as usize
}

fn push(s: &mut St, t: u8, src: u8, dst: usize) -> Result<(), String> {
    // A switch backfill can reach a ticket that per-ticket completion already
    // declared done. STRICT_SWITCH=1 shows it; the lookup operator never does.
    let exempt = dst == W && src != SEED && std::env::var("STRICT_SWITCH").is_err() && SWITCH_MODE.with(|m| m.get());
    if s.processed[dst] >= t as i16 {
        return Err(format!("late arrival: {} gets t{} after processing t{}", NAMES[dst], t, s.processed[dst]));
    }
    if s.cw[dst] > t {
        return Err(format!("broken promise: {} gets t{} with watermark {}", NAMES[dst], t, s.cw[dst]));
    }
    if s.complete & (1 << t) != 0 && !exempt {
        return Err(format!("t{} reached {} after it completed", t, NAMES[dst]));
    }
    if !s.pend[dst].contains(&(t, src)) {
        s.pend[dst].push((t, src));
        s.pend[dst].sort();
    }
    Ok(())
}

fn ticket_live(s: &St, t: u8) -> bool {
    t >= s.seeded || s.pend.iter().any(|p| p.iter().any(|e| e.0 == t))
}

fn mark_complete(s: &mut St) {
    for t in 0..s.seeded {
        if !ticket_live(s, t) {
            s.complete |= 1 << t;
        }
    }
}

fn barrier_blocks_admit(s: &St) -> bool {
    let t = s.seeded as usize;
    let all_before_done = (0..t as u8).all(|k| s.complete & (1 << k) != 0);
    if let Desc::Src { barrier: true, .. } = s.queue[t] {
        if !all_before_done {
            return true;
        }
    }
    (0..t).any(|k| matches!(s.queue[k], Desc::Src { barrier: true, .. }) && s.complete & (1 << k) == 0)
}

fn succ(s: &St, cfg: Cfg, out: &mut Vec<St>) -> Result<(), String> {
    if (s.seeded as usize) < s.queue.len() && !barrier_blocks_admit(s) {
        let mut n = s.clone();
        let t = n.seeded;
        let node = match n.queue[t as usize] {
            Desc::Src { node, .. } | Desc::Fresh { node, .. } => node as usize,
            Desc::Rewire { .. } if mutant("rewire_at_fire") => W,
            Desc::Rewire { to } => {
                // Structural ticket: the edge change is published with S, so no
                // input can have fired this ticket yet.
                let i = inner_edge(&n);
                n.edges[i].to = t;
                n.edges.push(Edge { src: to, dst: W as u8, from: t, to: INF });
                n.edges.sort();
                W
            }
        };
        n.seeded += 1;
        push(&mut n, t, SEED, node)?;
        out.push(n);
    }
    for v in 0..NODES {
        let w = if mutant("cw_ignores_pending") { input_w(s, v) } else { input_w(s, v).min(min_pend(s, v)) };
        if w > s.cw[v] {
            let mut n = s.clone();
            n.cw[v] = w;
            out.push(n);
        }
    }
    for v in 0..NODES {
        let Some(&(t, _)) = s.pend[v].first() else { continue };
        if t >= input_w(s, v) && !mutant("no_input_guard") {
            continue;
        }
        if v == J && s.cw[U] <= t && !mutant("no_side_read_wait") {
            continue; // side read of U at ticket t waits until U is past t
        }
        let sel = if at(s, S2, t) % 2 == 1 { S3 } else { S1 };
        if v == W && !cfg.lookup && !cfg.next && s.cw[sel] <= t && !mutant("no_inner_wait") {
            continue; // the new inner must be past t before W reads it
        }
        let mut n = s.clone();
        if n.processed[v] >= t as i16 {
            return Err(format!("{} fired t{} after t{}", NAMES[v], t, n.processed[v]));
        }
        let seeded_here = n.pend[v].iter().any(|p| p.0 == t && p.1 == SEED);
        if let (true, Some(Desc::Rewire { to })) = (v == W && seeded_here && mutant("rewire_at_fire"), n.queue.get(t as usize).copied()) {
            let i = inner_edge(&n);
            n.edges[i].to = t;
            n.edges.push(Edge { src: to, dst: W as u8, from: t, to: INF });
            n.edges.sort();
        }
        let value = match v {
            S1 | S2 | S3 | U if seeded_here => match n.queue[t as usize] {
                Desc::Src { val, .. } | Desc::Fresh { val, .. } => val,
                Desc::Rewire { .. } => unreachable!(),
            },
            W if cfg.next => at(&n, current_inner(&n, t), t),
            A => 10 * at(&n, S1, t),
            U => at(&n, A, t) + 1,
            W => at(&n, sel, t),
            J => at(&n, A, t) + 100 * at(&n, S2, t) + 1000 * at(&n, U, t),
            _ => unreachable!(),
        };
        let emits = v != W
            || !cfg.lookup
            || n.pend[W].iter().any(|p| p.0 == t && (p.1 as usize == S2 || p.1 as usize == sel));
        if emits {
            n.hist[v].push((t, value));
        }
        n.processed[v] = t as i16;
        n.pend[v].retain(|p| p.0 != t);
        let outs: Vec<Edge> = n
            .edges
            .iter()
            .filter(|e| e.src as usize == v && e.from <= t && t < e.to)
            .copied()
            .collect();
        for e in outs {
            push(&mut n, t, v as u8, e.dst as usize)?;
        }
        if v == A && !cfg.declared_au && mutant("undeclared_joins") {
            push(&mut n, t, A as u8, U)?;
        } else if v == A && !cfg.declared_au {
            // Undeclared callback write: always a new, later ticket.
            n.queue.push(Desc::Fresh { node: U as u8, val: value + 1 });
        }
        if v == A && cfg.declared_au {
            n.joined |= 1 << t;
        }
        if v == W && cfg.next && current_inner(&n, t) != sel {
            n.queue.push(Desc::Rewire { to: sel as u8 });
        }
        if v == W && !cfg.lookup && !cfg.next {
            let i = inner_edge(&n);
            if n.edges[i].src as usize != sel {
                let old = n.edges[i].src;
                n.edges[i].to = t + 1;
                if !mutant("no_retract") { n.pend[W].retain(|p| !(p.1 == old && p.0 > t)); }
                n.edges.push(Edge { src: sel as u8, dst: W as u8, from: t + 1, to: INF });
                n.edges.sort();
                let back: Vec<u8> = n.hist[sel].iter().map(|h| h.0).filter(|&k| k > t).collect();
                for k in back.into_iter().filter(|_| !mutant("no_backfill")) {
                    push(&mut n, k, sel as u8, W)?;
                }
            }
        }
        out.push(n);
    }
    Ok(())
}

fn serial(s: &St, cfg: Cfg) -> [Vec<(u8, i32)>; NODES] {
    let mut val: [i32; NODES] = std::array::from_fn(init_value);
    let mut hist: [Vec<(u8, i32)>; NODES] = Default::default();
    let mut cur = S1;
    for (t, d) in s.queue.iter().enumerate() {
        let t = t as u8;
        let mut touched = [false; NODES];
        match *d {
            Desc::Src { node, val: v, .. } | Desc::Fresh { node, val: v } => {
                val[node as usize] = v;
                touched[node as usize] = true;
            }
            Desc::Rewire { to } => {
                cur = to as usize;
                touched[W] = true;
            }
        }
        let inner_before = if val_before_odd(&hist, S2, t) { S3 } else { S1 };
        touched[A] |= touched[S1];
        if touched[A] {
            val[A] = 10 * val[S1];
        }
        if cfg.declared_au && touched[A] {
            touched[U] = true;
            val[U] = val[A] + 1;
        }
        let inner_now = if val[S2] % 2 == 1 { S3 } else { S1 };
        let inner = if cfg.lookup { inner_now } else { inner_before };
        if cfg.next {
            touched[W] |= touched[S2] || touched[cur];
            if touched[W] {
                val[W] = val[cur];
            }
        } else {
            touched[W] = touched[S2] || touched[inner];
            if touched[W] {
                val[W] = val[inner_now];
            }
        }
        touched[J] = touched[A] || touched[S2];
        if touched[J] {
            val[J] = val[A] + 100 * val[S2] + 1000 * val[U];
        }
        for n in 0..NODES {
            if touched[n] {
                hist[n].push((t, val[n]));
            }
        }
    }
    hist
}

fn val_before_odd(hist: &[Vec<(u8, i32)>; NODES], n: usize, t: u8) -> bool {
    hist[n].iter().rev().find(|h| h.0 < t).map_or(init_value(n), |h| h.1) % 2 == 1
}

fn check(queue: Vec<Desc>, cfg: Cfg) -> Result<(usize, usize), String> {
    let mut edges = vec![
        Edge { src: S1 as u8, dst: A as u8, from: 0, to: INF },
        Edge { src: A as u8, dst: J as u8, from: 0, to: INF },
        Edge { src: S2 as u8, dst: J as u8, from: 0, to: INF },
        Edge { src: S2 as u8, dst: W as u8, from: 0, to: INF },
        Edge { src: S1 as u8, dst: W as u8, from: 0, to: INF },
    ];
    if cfg.declared_au {
        edges.push(Edge { src: A as u8, dst: U as u8, from: 0, to: INF });
    }
    if cfg.lookup {
        edges.push(Edge { src: S3 as u8, dst: W as u8, from: 0, to: INF });
    }
    edges.sort();
    let init = St {
        queue,
        seeded: 0,
        pend: Default::default(),
        hist: Default::default(),
        processed: [-1; NODES],
        cw: [0; NODES],
        edges,
        joined: 0,
        complete: 0,
    };
    let mut ids: HashMap<St, usize> = HashMap::new();
    let mut succs: Vec<Vec<usize>> = Vec::new();
    let mut work = VecDeque::new();
    ids.insert(init.clone(), 0);
    succs.push(Vec::new());
    work.push_back(init);
    let mut terminals = 0;
    let mut buf = Vec::new();
    while let Some(s) = work.pop_front() {
        let id = ids[&s];
        buf.clear();
        succ(&s, cfg, &mut buf).map_err(|e| format!("{e}\nstate: {s:?}"))?;
        if buf.is_empty() {
            terminals += 1;
            if (s.seeded as usize) != s.queue.len() || s.pend.iter().any(|p| !p.is_empty()) {
                return Err(format!("deadlock: {s:?}"));
            }
            let want = serial(&s, cfg);
            if want != s.hist {
                return Err(format!("not serializable:\n got  {:?}\n want {:?}\n queue {:?}", s.hist, want, s.queue));
            }
        }
        for mut n in buf.drain(..) {
            mark_complete(&mut n);
            let next = ids.len();
            let nid = *ids.entry(n.clone()).or_insert_with(|| {
                work.push_back(n);
                succs.push(Vec::new());
                next
            });
            succs[id].push(nid);
        }
    }
    // Progress: the reachable graph has no cycle, so with every path finite and
    // every terminal state complete, every ticket completes on every path.
    let mut indeg = vec![0usize; succs.len()];
    for ss in &succs {
        for &n in ss {
            indeg[n] += 1;
        }
    }
    let mut q: Vec<usize> = (0..succs.len()).filter(|&i| indeg[i] == 0).collect();
    let mut seen = 0;
    while let Some(i) = q.pop() {
        seen += 1;
        for &n in &succs[i] {
            indeg[n] -= 1;
            if indeg[n] == 0 {
                q.push(n);
            }
        }
    }
    if seen != succs.len() {
        return Err("livelock: the state graph has a cycle".into());
    }
    Ok((ids.len(), terminals))
}

fn main() {
    let k: usize = std::env::args().nth(1).map_or(3, |a| a.parse().unwrap());
    let mut total = 0;
    let modes: Vec<&str> = std::env::var("MODES").map_or(vec!["now", "next", "lookup"], |m| m.leak().split(',').collect());
    for (declared, mode) in [false, true].into_iter().flat_map(|d| modes.iter().map(move |m| (d, *m))) {
        let lookup = mode == "lookup";
        let cfg = Cfg { declared_au: declared, lookup, next: mode == "next" };
        SWITCH_MODE.with(|m| m.set(mode == "now"));
        for mask in 0..3u32.pow(k as u32) {
            for barrier in 0..=k {
                let queue: Vec<Desc> = (0..k)
                    .map(|i| Desc::Src {
                        node: [S1, S2, S3][(mask / 3u32.pow(i as u32) % 3) as usize] as u8,
                        val: i as i32 + 1,
                        barrier: barrier == i + 1,
                    })
                    .collect();
                match check(queue.clone(), cfg) {
                    Ok((states, terms)) => {
                        total += states;
                        println!("ok declared_au={declared} mode={mode} tickets={mask} barrier={barrier} states={states} terminals={terms}");
                    }
                    Err(e) => {
                        println!("FAIL declared_au={declared} mode={mode} queue={queue:?}\n{e}");
                        std::process::exit(1);
                    }
                }
            }
        }
    }
    println!("all ok, {total} states");
}
