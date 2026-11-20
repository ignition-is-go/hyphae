// Exhaustive checker for quiet certificates (section 4 of
// hyphae/docs/settle-epochs.md). A node marked quiet promises that nothing in
// its ancestry, itself included, holds pending work, so its watermark follows
// S with no walk. Seeding a ticket clears the quiet marks of the seed's quiet
// descendants before S passes the ticket. A validator marks a chain quiet
// bottom-up, re-checking each mark. Every step of the seeder and the validator
// is its own atomic action, so they interleave with each other and with fires.
//
// Graph: X -> P -> B, Y -> B, B -> C, Z -> C. C joins an active input (Z) and
// a long-quiet input (B).
//
// GATE=1 models a family reader. Stores S and T feed a root R, the family
// node F hears R and the key K, and the reader L hears K, its row W (fed by
// S), and F along an edge gated by K: F counts toward L at ticket t only when
// K delivered t to L, and F writes L only then. C joins L and Z. Clearing and
// validation follow ungated edges only, so a seed at T, which L's row never
// reads, leaves L's mark alone.
use std::collections::{HashMap, VecDeque};

const X: usize = 0;
const Y: usize = 1;
const Z: usize = 2;
const P: usize = 3;
const B: usize = 4;
const C: usize = 5;
// GATE=1 nodes.
const S: usize = 0;
const T: usize = 1;
const K: usize = 3;
const R: usize = 4;
const F: usize = 6;
const W: usize = 7;
const L: usize = 8;
const NODES: usize = 9;
const NONE: u8 = 0;
const TENTATIVE: u8 = 1;
const QUIET: u8 = 2;
const EDGES: [(usize, usize); 5] = [(X, P), (P, B), (Y, B), (B, C), (Z, C)];
const VALIDATE: [usize; 4] = [X, Y, P, B]; // B's ancestry in height order
const GATE_EDGES: [(usize, usize); 9] =
    [(S, W), (S, R), (T, R), (R, F), (K, F), (K, L), (W, L), (L, C), (Z, C)];
/// F -> L, gated by K.
const GATED: (usize, usize, usize) = (F, L, K);
/// L's ungated ancestry, then F's: a reader's open gate validates F, so F and
/// its ancestry can hold marks a seed at T must clear.
const GATE_VALIDATE: [usize; 7] = [S, K, W, L, T, R, F];

fn gated() -> bool {
    std::env::var("GATE").is_ok_and(|g| g == "1")
}

fn mutant(name: &str) -> bool {
    std::env::var("MUTANT").is_ok_and(|m| m == name)
}

fn edges() -> &'static [(usize, usize)] {
    if gated() { &GATE_EDGES } else { &EDGES }
}

fn validate_chain() -> &'static [usize] {
    if gated() { &GATE_VALIDATE } else { &VALIDATE }
}

/// The node C reads as a long-quiet input.
fn quiet_input() -> usize {
    if gated() { L } else { B }
}

fn inputs(n: usize) -> impl Iterator<Item = usize> {
    edges().iter().filter(move |e| e.1 == n).map(|e| e.0)
}

/// Ungated outputs; with `clear_through_gate`, clearing also follows F -> L.
fn outputs(n: usize) -> impl Iterator<Item = usize> {
    let through = gated() && mutant("clear_through_gate") && n == GATED.0;
    edges()
        .iter()
        .filter(move |e| e.0 == n)
        .map(|e| e.1)
        .chain(through.then_some(GATED.1))
}

/// Data outputs of a fire at `t`: the ungated ones, and F -> L when K
/// delivered `t` to L.
fn pushes(s: &St, n: usize, t: u8) -> Vec<usize> {
    let mut out: Vec<usize> = edges().iter().filter(|e| e.0 == n).map(|e| e.1).collect();
    if gated() && n == GATED.0 && s.key_at.contains(&t) {
        out.push(GATED.1);
    }
    out
}

fn ancestry(n: usize, out: &mut Vec<usize>) {
    if !out.contains(&n) {
        out.push(n);
        for i in inputs(n) {
            ancestry(i, out);
        }
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
enum Admit {
    Idle,
    /// The seed is pending; the quiet descendants in `work` are still marked.
    Invalidating { ticket: u8, work: Vec<u8> },
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
enum Validate {
    Idle,
    Mark(u8),
    Write(u8),
    Recheck(u8),
    Promote(u8),
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct St {
    seeded: u8,
    pend: [Vec<u8>; NODES],
    hist: [Vec<(u8, i32)>; NODES],
    processed: [i16; NODES],
    cw: [u8; NODES],
    /// NONE, TENTATIVE (written, not yet rechecked), or QUIET. Only QUIET is
    /// trusted by readers.
    quiet: [u8; NODES],
    admit: Admit,
    validate: Validate,
    /// The source of the ticket being seeded, while it is.
    seed_at: Option<u8>,
    /// GATE=1: the tickets K delivered to L.
    key_at: Vec<u8>,
}

fn at(s: &St, n: usize, t: u8) -> i32 {
    s.hist[n].iter().rev().find(|h| h.0 <= t).map_or(0, |h| h.1)
}

/// What input `i` contributes to a dependent's input watermark. A quiet
/// input follows S in O(1); otherwise its cached numeric watermark is used.
fn contrib(s: &St, i: usize) -> u8 {
    if s.quiet[i] == QUIET { s.seeded } else { s.cw[i] }
}

fn input_w(s: &St, n: usize) -> u8 {
    inputs(n).map(|i| contrib(s, i)).fold(s.seeded, u8::min)
}

/// The fire rule at `t`, with F counted toward L only when the gate is open.
fn may_fire(s: &St, n: usize, t: u8) -> bool {
    if t >= input_w(s, n) {
        return false;
    }
    if gated() && n == GATED.1 && s.key_at.contains(&t) && !mutant("quiet_ignores_gate") {
        return contrib(s, GATED.0) > t;
    }
    true
}

fn min_pend(s: &St, n: usize) -> u8 {
    s.pend[n].first().copied().unwrap_or(u8::MAX)
}

/// A quiet node has no pending work below S anywhere in its ancestry.
fn check_quiet(s: &St) -> Result<(), String> {
    for n in 0..NODES {
        if s.quiet[n] != QUIET {
            continue;
        }
        let mut anc = Vec::new();
        ancestry(n, &mut anc);
        for a in anc {
            if s.pend[a].iter().any(|&t| t < s.seeded) {
                return Err(format!("node {n} is quiet while node {a} holds {:?} below S={}", s.pend[a], s.seeded));
            }
        }
        // A gate is open only at a ticket K delivered to L, which L then
        // holds, so a quiet L has no open gate.
        if gated() && n == GATED.1 && s.key_at.iter().any(|t| s.pend[L].contains(t)) {
            return Err(format!("L is quiet while it holds a key ticket {:?}", s.pend[L]));
        }
    }
    Ok(())
}

fn push(s: &mut St, t: u8, dst: usize) -> Result<(), String> {
    if s.processed[dst] >= t as i16 || s.cw[dst] > t {
        return Err(format!("unsound push of t{t} to node {dst}"));
    }
    if !s.pend[dst].contains(&t) {
        s.pend[dst].push(t);
        s.pend[dst].sort();
    }
    Ok(())
}

fn succ(s: &St, q: &[usize], out: &mut Vec<St>) -> Result<(), String> {
    // Admission: seed, clear quiet marks one node per step, then publish S.
    match &s.admit {
        Admit::Idle if (s.seeded as usize) < q.len() => {
            let mut n = s.clone();
            let t = n.seeded;
            let x = q[t as usize];
            push(&mut n, t, x)?;
            n.quiet[x] = NONE;
            n.seed_at = Some(x as u8);
            let work: Vec<u8> = outputs(x).map(|d| d as u8).collect();
            if mutant("publish_before_invalidate") {
                n.seeded += 1;
                n.admit = Admit::Invalidating { ticket: t, work };
            } else {
                n.admit = Admit::Invalidating { ticket: t, work };
            }
            out.push(n);
        }
        Admit::Invalidating { ticket, work } => {
            let mut n = s.clone();
            let mut work = work.clone();
            if let Some(d) = work.pop() {
                let d = d as usize;
                if gated() && d == L && n.seed_at == Some(T as u8) && n.quiet[L] != NONE {
                    return Err("a seed at T, which L's row never reads, cleared L's mark".into());
                }
                // Stop at a node that is not quiet: by closure nothing below it is.
                if n.quiet[d] != NONE {
                    n.quiet[d] = NONE;
                    if !mutant("invalidate_one_level") {
                        work.extend(outputs(d).map(|o| o as u8));
                    }
                }
                n.admit = Admit::Invalidating { ticket: *ticket, work };
            } else {
                if !mutant("publish_before_invalidate") {
                    n.seeded = ticket + 1;
                }
                n.admit = Admit::Idle;
                n.seed_at = None;
            }
            out.push(n);
        }
        Admit::Idle => {}
    }
    // Validation of C's quiet input B, started on demand.
    match s.validate {
        Validate::Idle => {
            if !s.pend[C].is_empty() && s.quiet[quiet_input()] != QUIET {
                let mut n = s.clone();
                n.validate = Validate::Mark(0);
                out.push(n);
            }
        }
        Validate::Mark(i) => {
            let mut n = s.clone();
            let v = validate_chain()[i as usize];
            let ok = n.pend[v].is_empty() && (mutant("mark_ignores_inputs") || inputs(v).all(|u| n.quiet[u] == QUIET));
            n.validate = if ok { Validate::Write(i) } else { Validate::Idle };
            out.push(n);
        }
        Validate::Write(i) => {
            // The mark is written in a separate step from the read that
            // justified it, as on real hardware.
            let mut n = s.clone();
            let v = validate_chain()[i as usize];
            {
                n.quiet[v] = if mutant("no_recheck") { QUIET } else { TENTATIVE };
                n.validate = if mutant("no_recheck") {
                    if (i as usize) + 1 < validate_chain().len() { Validate::Mark(i + 1) } else { Validate::Idle }
                } else {
                    Validate::Recheck(i)
                };
            }
            out.push(n);
        }
        Validate::Recheck(i) => {
            let mut n = s.clone();
            let v = validate_chain()[i as usize];
            if n.pend[v].is_empty() && (mutant("mark_ignores_inputs") || inputs(v).all(|u| n.quiet[u] == QUIET)) {
                n.validate = Validate::Promote(i);
            } else {
                if n.quiet[v] == TENTATIVE {
                    n.quiet[v] = NONE;
                }
                n.validate = Validate::Idle;
            }
            out.push(n);
        }
        Validate::Promote(i) => {
            // compare_exchange(TENTATIVE, QUIET): fails if a seeder cleared it.
            let mut n = s.clone();
            let v = validate_chain()[i as usize];
            if n.quiet[v] == TENTATIVE || mutant("blind_promote") {
                n.quiet[v] = QUIET;
                n.validate = if (i as usize) + 1 < validate_chain().len() { Validate::Mark(i + 1) } else { Validate::Idle };
            } else {
                n.validate = Validate::Idle;
            }
            out.push(n);
        }
    }
    // Numeric watermark refresh: the slow path the quiet mark replaces.
    for v in 0..NODES {
        let w = input_w(s, v).min(min_pend(s, v));
        if w > s.cw[v] {
            let mut n = s.clone();
            n.cw[v] = w;
            out.push(n);
        }
    }
    for v in 0..NODES {
        let Some(&t) = s.pend[v].first() else { continue };
        if !may_fire(s, v, t) {
            continue;
        }
        let mut n = s.clone();
        if n.processed[v] >= t as i16 {
            return Err(format!("node {v} fired t{t} out of order"));
        }
        n.processed[v] = t as i16;
        n.pend[v].retain(|&p| p != t);
        let value = value(&n, v, t);
        n.hist[v].push((t, value));
        if gated() && v == K {
            n.key_at.push(t);
        }
        for d in pushes(&n, v, t) {
            if n.quiet[d] == QUIET {
                return Err(format!("push of t{t} into quiet node {d}"));
            }
            push(&mut n, t, d)?;
        }
        out.push(n);
    }
    Ok(())
}

fn value(n: &St, v: usize, t: u8) -> i32 {
    if gated() {
        return match v {
            S | T | K | Z => t as i32 + 1,
            R => at(n, S, t) + at(n, T, t),
            F => at(n, R, t) + at(n, K, t),
            W => at(n, S, t),
            L => at(n, W, t) + 100 * at(n, K, t),
            C => at(n, Z, t) + 1000 * at(n, L, t),
            _ => unreachable!(),
        };
    }
    match v {
        X | Y | Z => t as i32 + 1,
        P => at(n, X, t),
        B => at(n, P, t) + 10 * at(n, Y, t),
        C => at(n, Z, t) + 1000 * at(n, B, t),
        _ => 0,
    }
}

fn serial(q: &[usize]) -> [Vec<(u8, i32)>; NODES] {
    if gated() {
        return serial_gated(q);
    }
    let mut val = [0i32; NODES];
    let mut hist: [Vec<(u8, i32)>; NODES] = Default::default();
    for (t, &x) in q.iter().enumerate() {
        let mut touched = [false; NODES];
        val[x] = t as i32 + 1;
        touched[x] = true;
        touched[P] = touched[X];
        val[P] = val[X];
        touched[B] = touched[P] || touched[Y];
        val[B] = val[P] + 10 * val[Y];
        touched[C] = touched[B] || touched[Z];
        val[C] = val[Z] + 1000 * val[B];
        for n in 0..NODES {
            if touched[n] {
                hist[n].push((t as u8, val[n]));
            }
        }
    }
    hist
}

/// GATE=1: each ticket's touched nodes fire once, in topological order.
fn serial_gated(q: &[usize]) -> [Vec<(u8, i32)>; NODES] {
    let order = [S, T, K, Z, R, W, F, L, C];
    let mut s = St {
        seeded: 0,
        pend: Default::default(),
        hist: Default::default(),
        processed: [-1; NODES],
        cw: [0; NODES],
        quiet: [NONE; NODES],
        admit: Admit::Idle,
        validate: Validate::Idle,
        seed_at: None,
        key_at: Vec::new(),
    };
    for (t, &x) in q.iter().enumerate() {
        let t = t as u8;
        let mut touched = [false; NODES];
        touched[x] = true;
        for &v in &order {
            if !touched[v] {
                continue;
            }
            let val = value(&s, v, t);
            s.hist[v].push((t, val));
            if v == K {
                s.key_at.push(t);
            }
            for d in pushes(&s, v, t) {
                touched[d] = true;
            }
        }
    }
    s.hist
}

fn check(q: &[usize]) -> Result<usize, String> {
    let init = St {
        seeded: 0,
        pend: Default::default(),
        hist: Default::default(),
        processed: [-1; NODES],
        cw: [0; NODES],
        quiet: [NONE; NODES],
        admit: Admit::Idle,
        validate: Validate::Idle,
        seed_at: None,
        key_at: Vec::new(),
    };
    let want = serial(q);
    let mut ids: HashMap<St, usize> = HashMap::new();
    let mut preds: Vec<Vec<usize>> = vec![Vec::new()];
    let mut terminal = Vec::new();
    let mut work = VecDeque::from([init.clone()]);
    ids.insert(init, 0);
    let mut buf = Vec::new();
    while let Some(s) = work.pop_front() {
        let id = ids[&s];
        check_quiet(&s).map_err(|e| format!("{e}\n{s:?}"))?;
        buf.clear();
        succ(&s, q, &mut buf).map_err(|e| format!("{e}\n{s:?}"))?;
        let done = (s.seeded as usize) == q.len() && s.pend.iter().all(Vec::is_empty) && s.admit == Admit::Idle;
        if done {
            if s.hist != want {
                return Err(format!("not serializable: got {:?} want {:?}", s.hist, want));
            }
            terminal.push(id);
        } else if buf.is_empty() {
            return Err(format!("deadlock {s:?}"));
        }
        for n in buf.drain(..) {
            let next = ids.len();
            let nid = *ids.entry(n.clone()).or_insert_with(|| {
                work.push_back(n);
                preds.push(Vec::new());
                next
            });
            preds[nid].push(id);
        }
    }
    // Progress: from every reachable state some path completes every ticket.
    // Validation may fail and retry, so this is checked under fairness rather
    // than by acyclicity.
    let mut ok = vec![false; preds.len()];
    let mut stack = terminal;
    for &t in &stack {
        ok[t] = true;
    }
    while let Some(i) = stack.pop() {
        for &p in &preds[i] {
            if !ok[p] {
                ok[p] = true;
                stack.push(p);
            }
        }
    }
    if let Some(bad) = ok.iter().position(|&b| !b) {
        return Err(format!("no path to completion from state {bad}"));
    }
    Ok(ids.len())
}

fn main() {
    let k: u32 = std::env::args().nth(1).map_or(3, |a| a.parse().unwrap());
    let (mut total, mut configs) = (0, 0);
    let sources: &[usize] = if gated() { &[S, T, K, Z] } else { &[X, Y, Z] };
    let n = sources.len() as u32;
    for mask in 0..n.pow(k) {
        let q: Vec<usize> = (0..k).map(|t| sources[(mask / n.pow(t) % n) as usize]).collect();
        match check(&q) {
            Ok(n) => {
                total += n;
                configs += 1;
            }
            Err(e) => {
                println!("FAIL {q:?}\n{e}");
                std::process::exit(1);
            }
        }
    }
    println!("all ok, {configs} ticket sequences, {total} states");
}
