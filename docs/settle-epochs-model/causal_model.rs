// Exhaustive checker for the causal readiness rule in
// hyphae/docs/settle-epochs.md, section 14. There are no tickets. A write
// belongs to a writer and carries the writes that must precede it. A node fires
// a closed group of the writes pending at it once every member, and every
// earlier write of each member, has no pending entry below the node's height.
//
// SCEN=diamond: roots X, Y and U. P and Q both join X and Y, and D joins P, Q
// and U. P's callback writes U (writer 2) when it fires a trigger write.
// SCEN=family: store I, demand K (a set over two keys), family Fam with rows F0
// and F1 built and torn down inside Fam's fire, and reader L = Fam.
//
// The rule's code reads only the node's own queues and, per write, the lowest
// height holding it (`minh`). The checks use a global oracle (`will_reach`) to
// judge it.
use std::collections::{HashMap, VecDeque};

const SEED: u8 = 254;
const INF: u8 = 255;
const NODES: usize = 6;
const WRITERS: usize = 2;

const X: usize = 0;
const Y: usize = 1;
const U: usize = 2;
const P: usize = 3;
const Q: usize = 4;
const D: usize = 5;

const I: usize = 0;
const K: usize = 1;
const FAM: usize = 2;
const L: usize = 3;
const F0: usize = 4;
const KEYS: usize = 2;

fn mutant(name: &str) -> bool {
    std::env::var("MUTANT").is_ok_and(|m| m == name)
}

fn skip() -> bool {
    std::env::var("SKIP").is_ok_and(|s| s == "1")
}

fn family() -> bool {
    std::env::var("SCEN").is_ok_and(|s| s == "family")
}

fn height(n: usize) -> u8 {
    if family() { [0, 0, 2, 3, 1, 1][n] } else { [0, 0, 0, 1, 1, 2][n] }
}

#[derive(Clone, Copy, Debug, Default, Hash, PartialEq, Eq)]
struct View {
    mask: u16,
    val: i32,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
struct Edge {
    src: u8,
    dst: u8,
    q: Vec<(u16, i32, u16)>, // (grp, view.val, view.mask)
    last: (i32, u16),
    /// Ghost, SKIP=1 only: the writes `src` has applied without telling
    /// `dst`, so the view `last` is also the value after these.
    silent: u16,
}

impl Edge {
    fn new(src: usize, dst: usize, last: View) -> Self {
        Edge { src: src as u8, dst: dst as u8, q: Vec::new(), last: (last.val, last.mask), silent: 0 }
    }
    fn last(&self) -> View {
        View { val: self.last.0, mask: self.last.1 }
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct Wr {
    writer: u8,
    admitted: bool,
    trigger: bool,
    /// The earlier writes the rule waits for (mutants drop some).
    rule: u16,
    /// The earlier writes properties 1 and 2 require, transitively closed.
    truth: u16,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct St {
    next: [u8; WRITERS],
    writes: Vec<Wr>,
    view: [View; NODES],
    applied: [u16; NODES],
    edges: Vec<Edge>,
    built: [bool; KEYS],
}

/// One user write: the roots it sets and their values.
#[derive(Clone, Debug)]
struct Act {
    roots: Vec<(usize, i32)>,
    trigger: bool,
}

struct Cfg {
    /// Write ids of each user writer, in send order.
    lists: [Vec<usize>; WRITERS],
    acts: Vec<Act>,
}

fn bits(m: u16) -> impl Iterator<Item = usize> {
    (0..16).filter(move |b| m & (1 << b) != 0)
}

fn row(store: i32, k: usize) -> i32 {
    10 * store + k as i32 + 1
}

/// The lowest height holding a pending entry of write `x`; INF once it has
/// none. Admission is atomic, so an admitted write with none is complete.
fn minh(s: &St, x: usize) -> u8 {
    s.edges
        .iter()
        .filter(|e| e.q.iter().any(|d| d.0 & (1 << x) != 0))
        .map(|e| height(e.dst as usize))
        .min()
        .unwrap_or(INF)
}

fn reaches(s: &St, from: usize, to: usize) -> bool {
    let mut seen = [false; NODES];
    let mut stack = vec![from];
    while let Some(n) = stack.pop() {
        if n == to {
            return true;
        }
        if std::mem::replace(&mut seen[n], true) {
            continue;
        }
        stack.extend(s.edges.iter().filter(|e| e.src as usize == n).map(|e| e.dst as usize));
    }
    false
}

/// Oracle: write `x` can still arrive at `target`.
fn will_reach(s: &St, x: usize, target: usize) -> bool {
    s.edges.iter().any(|e| e.q.iter().any(|d| d.0 & (1 << x) != 0) && reaches(s, e.dst as usize, target))
}

fn ins(s: &St, n: usize) -> impl Iterator<Item = &Edge> {
    s.edges.iter().filter(move |e| e.dst as usize == n)
}

/// The smallest group containing `x` that is closed under each in-edge's FIFO
/// prefix and under the rule's predecessors pending here.
fn closure(s: &St, n: usize, x: usize) -> u16 {
    let pend: u16 = ins(s, n).flat_map(|e| e.q.iter().map(|d| d.0)).fold(0, |a, b| a | b);
    let mut c = 1u16 << x;
    loop {
        let old = c;
        for e in ins(s, n) {
            if let Some(i) = e.q.iter().rposition(|d| d.0 & c != 0) {
                // fifo_strict: no merging; the node may only take queue heads.
                if mutant("fifo_strict") {
                    continue;
                }
                c |= e.q[..=i].iter().fold(0, |a, d| a | d.0);
            }
        }
        for m in bits(c) {
            c |= s.writes[m].rule & pend;
        }
        if c == old {
            return c;
        }
    }
}

/// The readiness rule. Local: the node's queues, plus `minh` of the group's
/// members and of their earlier writes.
fn ready(s: &St, n: usize, c: u16) -> bool {
    let h = height(n);
    if mutant("fifo_strict")
        && ins(s, n).any(|e| e.q.iter().rposition(|d| d.0 & c != 0).is_some_and(|i| e.q[..=i].iter().any(|d| d.0 & !c != 0)))
    {
        return false;
    }
    bits(c).all(|m| {
        (mutant("half_write") || minh(s, m) >= h) && bits(s.writes[m].rule & !c).all(|p| minh(s, p) >= h)
    })
}

fn push(s: &mut St, src: usize, dst: usize, grp: u16, view: View) -> Result<(), String> {
    if s.applied[dst] & grp != 0 {
        return Err(format!("node {dst} receives a write it already applied ({grp:#b})"));
    }
    let e = s.edges.iter_mut().find(|e| e.src as usize == src && e.dst as usize == dst).unwrap();
    e.q.push((grp, view.val, view.mask));
    Ok(())
}

fn admit(s: &mut St, cfg: &Cfg, x: usize) -> Result<(), String> {
    s.writes[x].admitted = true;
    for &(r, v) in &cfg.acts[x].roots {
        push(s, SEED as usize, r, 1 << x, View { mask: 1 << x, val: v })?;
    }
    Ok(())
}

/// Feedback from P's callback: writer 2, after every earlier writer-2 write
/// and after the writes that caused it.
fn feedback(s: &mut St, causes: u16) -> Result<(), String> {
    let id = s.writes.len();
    if id >= 16 {
        return Err("write cap".into());
    }
    let earlier: u16 = (0..id).filter(|&w| s.writes[w].writer == 2).fold(0, |a, w| a | 1 << w);
    let close = |m: u16, f: fn(&Wr) -> u16| bits(m).fold(m, |a, w| a | f(&s.writes[w]));
    let truth = close(earlier, |w| w.truth) | close(causes, |w| w.truth);
    let mut rule = 0;
    if !mutant("no_writer_order") {
        rule |= close(earlier, |w| w.rule);
    }
    if !mutant("feedback_overtakes") {
        rule |= close(causes, |w| w.rule);
    }
    s.writes.push(Wr { writer: 2, admitted: true, trigger: false, rule, truth });
    let val = 100 + id as i32;
    push(s, SEED as usize, U, 1 << id, View { mask: 1 << id, val })
}

fn fire(s: &St, n: usize, c: u16) -> Result<St, String> {
    let mut t = s.clone();
    if t.applied[n] & c != 0 {
        return Err(format!("node {n} applies a write twice"));
    }
    // Properties 1 and 2: every required earlier write is applied here, in
    // this group, or can never arrive.
    for m in bits(c) {
        for p in bits(t.writes[m].truth & !c) {
            if t.applied[n] & (1 << p) == 0 && will_reach(&t, p, n) {
                let why = if t.writes[p].writer == t.writes[m].writer { "per-writer order" } else { "causality" };
                return Err(format!("{why}: node {n} applies w{m} before w{p}"));
            }
        }
    }
    let mut seed = None;
    for e in t.edges.iter_mut().filter(|e| e.dst as usize == n) {
        if let Some(i) = e.q.iter().rposition(|d| d.0 & c != 0) {
            let (_, val, mask) = e.q[i];
            e.q.drain(..=i);
            e.last = (val, mask);
            e.silent = 0;
            if e.src == SEED {
                seed = Some(val);
            }
        }
    }
    // Property 3: a write in any input's view is in every input's view that
    // it has reached or will reach.
    // A silent write counts as in the view or out of it, whichever is needed:
    // the value is the same either way.
    let views: Vec<(usize, View, u16)> =
        ins(&t, n).filter(|e| e.src != SEED).map(|e| (e.src as usize, e.last(), e.silent)).collect();
    let union = views.iter().fold(0, |a, v| a | v.1.mask);
    for x in bits(union) {
        for &(src, v, silent) in &views {
            if (v.mask | silent) & (1 << x) == 0 && (t.applied[src] & (1 << x) != 0 || will_reach(&t, x, src)) {
                return Err(format!("glitch: node {n} fires with w{x} from some inputs but not node {src}"));
            }
        }
    }
    t.applied[n] |= c;
    let last = |t: &St, src: usize| ins(t, n).find(|e| e.src as usize == src).map(|e| e.last());
    let val = if seed.is_some() || t.edges.iter().any(|e| e.dst as usize == n && e.src == SEED) {
        seed.unwrap_or(t.view[n].val)
    } else if family() {
        match n {
            FAM => fam_fire(&mut t)?,
            L => last(&t, FAM).unwrap().val,
            _ => row(last(&t, I).unwrap().val, n - F0),
        }
    } else {
        let v = |src: usize| last(&t, src).unwrap().val;
        match n {
            P => (v(X) + v(Y)) % 3,
            Q => v(X) % 2,
            _ => v(P) + 10 * v(Q) + 100 * v(U),
        }
    };
    let changed = val != t.view[n].val;
    t.view[n] = View { mask: t.applied[n], val };
    if !family() && n == P {
        let causes = bits(c).filter(|&m| t.writes[m].trigger).fold(0, |a, m| a | 1 << m);
        if causes != 0 {
            feedback(&mut t, causes)?;
        }
    }
    let outs: Vec<usize> =
        t.edges.iter().filter(|e| e.src as usize == n).map(|e| e.dst as usize).collect();
    let root = t.edges.iter().any(|e| e.dst as usize == n && e.src == SEED);
    for d in outs {
        let v = t.view[n];
        // SKIP=1: a derived node whose value did not change pushes nothing.
        // Its readers' views of it are equal to the new value already; the
        // ghost mask update below only keeps the checker's bookkeeping exact.
        let e = t.edges.iter_mut().find(|e| e.src as usize == n && e.dst as usize == d).unwrap();
        if skip() && !changed && e.q.is_empty() && !root {
            e.silent |= c;
            continue;
        }
        // The next real delivery carries the writes skipped before it: its
        // value reflects them. skip_carry drops that.
        // The reader drops any it already applied through another input.
        let carried = e.silent & !t.applied[d];
        e.silent &= t.applied[d];
        let grp = if mutant("skip_carry") { c } else { c | carried };
        push(&mut t, n, d, grp, v)?;
    }
    Ok(t)
}

/// Fam's fire: build and tear down rows inside it, then combine the demanded
/// rows. A new row starts from the anchor's view and is backfilled with the
/// anchor's queue, which holds exactly the store writes Fam has not applied.
fn fam_fire(t: &mut St) -> Result<i32, String> {
    let anchor = t.edges.iter().find(|e| e.src as usize == I && e.dst as usize == FAM).cloned();
    let demand = t.edges.iter().find(|e| e.src as usize == K && e.dst as usize == FAM).unwrap().last().val;
    let mut sum = 0;
    for k in 0..KEYS {
        let f = F0 + k;
        let wanted = demand & (1 << k) != 0;
        if wanted && !t.built[k] {
            t.built[k] = true;
            let (from, q) = match &anchor {
                Some(a) => (a.last(), if mutant("no_backfill") { Vec::new() } else { a.q.clone() }),
                None => (t.view[I], Vec::new()),
            };
            t.view[f] = View { mask: from.mask, val: row(from.val, k) };
            t.applied[f] = from.mask;
            let mut feed = Edge::new(I, f, from);
            feed.q = q;
            t.edges.push(feed);
            t.edges.push(Edge::new(f, FAM, t.view[f]));
            t.edges.sort();
        } else if !wanted && t.built[k] {
            t.built[k] = false;
            t.edges.retain(|e| e.src as usize != f && e.dst as usize != f);
            t.view[f] = View::default();
            t.applied[f] = 0;
        }
        if wanted {
            let r = t.edges.iter().find(|e| e.src as usize == f && e.dst as usize == FAM).unwrap().last();
            if let Some(a) = &anchor {
                if r.mask != a.last().mask || r.val != row(a.last().val, k) {
                    return Err(format!("row {k} is not aligned with the store Fam applied"));
                }
            }
            sum += r.val;
        }
    }
    Ok(sum)
}

fn succ(s: &St, cfg: &Cfg, out: &mut Vec<St>) -> Result<(), String> {
    for w in 0..WRITERS {
        if let Some(&x) = cfg.lists[w].get(s.next[w] as usize) {
            let mut t = s.clone();
            t.next[w] += 1;
            admit(&mut t, cfg, x)?;
            out.push(t);
        }
    }
    for n in 0..NODES {
        let pend: u16 = ins(s, n).flat_map(|e| e.q.iter().map(|d| d.0)).fold(0, |a, b| a | b);
        let mut tried = Vec::new();
        for x in bits(pend) {
            let c = closure(s, n, x);
            if tried.contains(&c) {
                continue;
            }
            tried.push(c);
            if ready(s, n, c) {
                out.push(fire(s, n, c)?);
            }
        }
    }
    Ok(())
}

fn terminal(s: &St, cfg: &Cfg) -> Result<(), String> {
    if (0..WRITERS).any(|w| (s.next[w] as usize) < cfg.lists[w].len()) || s.edges.iter().any(|e| !e.q.is_empty()) {
        return Err(format!("deadlock {s:?}"));
    }
    if family() {
        let want: i32 = (0..KEYS).filter(|k| s.view[K].val & (1 << k) != 0).map(|k| row(s.view[I].val, k)).sum();
        if s.view[L].val != want {
            return Err(format!("L converged to {} want {want}", s.view[L].val));
        }
    } else {
        let (x, y) = (s.view[X].val, s.view[Y].val);
        let want = (x + y) % 3 + 10 * (x % 2) + 100 * s.view[U].val;
        let all = s.applied[X] | s.applied[Y] | s.applied[U];
        if s.view[D].val != want || (!skip() && s.applied[D] != all) {
            return Err(format!("diamond converged to {} (applied {:#b}) want {want} (applied {all:#b})", s.view[D].val, s.applied[D]));
        }
    }
    Ok(())
}

fn check(cfg: &Cfg) -> Result<usize, String> {
    let z = View::default();
    let mut edges = Vec::new();
    if family() {
        edges.extend([Edge::new(SEED as usize, I, z), Edge::new(SEED as usize, K, z), Edge::new(K, FAM, z), Edge::new(FAM, L, z)]);
        if !mutant("no_anchor") {
            edges.push(Edge::new(I, FAM, z));
        }
    } else {
        for r in [X, Y, U] {
            edges.push(Edge::new(SEED as usize, r, z));
        }
        for (a, b) in [(X, P), (Y, P), (X, Q), (Y, Q), (P, D), (Q, D), (U, D)] {
            edges.push(Edge::new(a, b, z));
        }
    }
    edges.sort();
    let mut writes = vec![Wr { writer: 0, admitted: false, trigger: false, rule: 0, truth: 0 }; cfg.acts.len()];
    for (w, list) in cfg.lists.iter().enumerate() {
        for (i, &x) in list.iter().enumerate() {
            let earlier = list[..i].iter().fold(0u16, |a, &e| a | 1 << e);
            let rule = if mutant("no_writer_order") { 0 } else { earlier };
            writes[x] = Wr { writer: w as u8, admitted: false, trigger: cfg.acts[x].trigger, rule, truth: earlier };
        }
    }
    let init = St { next: [0; WRITERS], writes, view: [z; NODES], applied: [0; NODES], edges, built: [false; KEYS] };
    let mut ids: HashMap<St, usize> = HashMap::new();
    let mut succs: Vec<Vec<usize>> = vec![Vec::new()];
    let mut work = VecDeque::from([init.clone()]);
    ids.insert(init, 0);
    let mut buf = Vec::new();
    while let Some(s) = work.pop_front() {
        let id = ids[&s];
        buf.clear();
        succ(&s, cfg, &mut buf).map_err(|e| format!("{e}\n{s:?}"))?;
        if buf.is_empty() {
            terminal(&s, cfg)?;
        }
        for n in buf.drain(..) {
            let next = ids.len();
            let nid = *ids.entry(n.clone()).or_insert_with(|| {
                work.push_back(n);
                succs.push(Vec::new());
                next
            });
            succs[id].push(nid);
        }
    }
    let mut indeg = vec![0usize; succs.len()];
    succs.iter().flatten().for_each(|&n| indeg[n] += 1);
    let mut stack: Vec<usize> = (0..succs.len()).filter(|&i| indeg[i] == 0).collect();
    let mut seen = 0;
    while let Some(i) = stack.pop() {
        seen += 1;
        for &n in &succs[i] {
            indeg[n] -= 1;
            if indeg[n] == 0 {
                stack.push(n);
            }
        }
    }
    if seen != succs.len() {
        return Err("livelock".into());
    }
    Ok(ids.len())
}

/// Letter `l` for the write with id `id`.
fn act(l: u32, id: usize) -> Act {
    let v = id as i32 + 1;
    if family() {
        match l {
            0 => Act { roots: vec![(I, v)], trigger: false },
            m @ 1..=4 => Act { roots: vec![(K, (m - 1) as i32)], trigger: false },
            m => Act { roots: vec![(I, v), (K, (m - 5) as i32)], trigger: false },
        }
    } else {
        match l {
            0 => Act { roots: vec![(X, v)], trigger: false },
            1 => Act { roots: vec![(Y, v)], trigger: false },
            2 => Act { roots: vec![(X, v), (Y, v)], trigger: false },
            _ => Act { roots: vec![(X, v)], trigger: true },
        }
    }
}

fn main() {
    // usage: causal_model A B: writer 0 sends A writes, writer 1 sends B.
    let arg = |i: usize| std::env::args().nth(i).map_or(2, |a| a.parse::<u32>().unwrap());
    let (a, b) = (arg(1), arg(2));
    let letters: u32 = if family() { 9 } else { 4 };
    let (mut total, mut configs) = (0, 0);
    for mask in 0..letters.pow(a + b) {
        let acts: Vec<Act> = (0..a + b).map(|i| act(mask / letters.pow(i) % letters, i as usize)).collect();
        let cfg = Cfg { lists: [(0..a as usize).collect(), (a as usize..(a + b) as usize).collect()], acts };
        match check(&cfg) {
            Ok(n) => {
                total += n;
                configs += 1;
            }
            Err(e) => {
                let seq: Vec<_> = cfg.acts.iter().map(|a| (a.roots.clone(), a.trigger)).collect();
                println!("FAIL {seq:?}\n{e}");
                std::process::exit(1);
            }
        }
    }
    println!("all ok, {configs} write sequences, {total} states");
}
