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
use std::collections::{HashMap, VecDeque};

const X: usize = 0;
const Y: usize = 1;
const Z: usize = 2;
const P: usize = 3;
const B: usize = 4;
const C: usize = 5;
const NODES: usize = 6;
const NONE: u8 = 0;
const TENTATIVE: u8 = 1;
const QUIET: u8 = 2;
const EDGES: [(usize, usize); 5] = [(X, P), (P, B), (Y, B), (B, C), (Z, C)];
const VALIDATE: [usize; 4] = [X, Y, P, B]; // B's ancestry in height order

fn mutant(name: &str) -> bool {
    std::env::var("MUTANT").is_ok_and(|m| m == name)
}

fn inputs(n: usize) -> impl Iterator<Item = usize> {
    EDGES.iter().filter(move |e| e.1 == n).map(|e| e.0)
}

fn outputs(n: usize) -> impl Iterator<Item = usize> {
    EDGES.iter().filter(move |e| e.0 == n).map(|e| e.1)
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
            }
            out.push(n);
        }
        Admit::Idle => {}
    }
    // Validation of C's quiet input B, started on demand.
    match s.validate {
        Validate::Idle => {
            if !s.pend[C].is_empty() && s.quiet[B] != QUIET {
                let mut n = s.clone();
                n.validate = Validate::Mark(0);
                out.push(n);
            }
        }
        Validate::Mark(i) => {
            let mut n = s.clone();
            let v = VALIDATE[i as usize];
            let ok = n.pend[v].is_empty() && (mutant("mark_ignores_inputs") || inputs(v).all(|u| n.quiet[u] == QUIET));
            n.validate = if ok { Validate::Write(i) } else { Validate::Idle };
            out.push(n);
        }
        Validate::Write(i) => {
            // The mark is written in a separate step from the read that
            // justified it, as on real hardware.
            let mut n = s.clone();
            let v = VALIDATE[i as usize];
            {
                n.quiet[v] = if mutant("no_recheck") { QUIET } else { TENTATIVE };
                n.validate = if mutant("no_recheck") {
                    if (i as usize) + 1 < VALIDATE.len() { Validate::Mark(i + 1) } else { Validate::Idle }
                } else {
                    Validate::Recheck(i)
                };
            }
            out.push(n);
        }
        Validate::Recheck(i) => {
            let mut n = s.clone();
            let v = VALIDATE[i as usize];
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
            let v = VALIDATE[i as usize];
            if n.quiet[v] == TENTATIVE || mutant("blind_promote") {
                n.quiet[v] = QUIET;
                n.validate = if (i as usize) + 1 < VALIDATE.len() { Validate::Mark(i + 1) } else { Validate::Idle };
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
        if t >= input_w(s, v) {
            continue;
        }
        let mut n = s.clone();
        if n.processed[v] >= t as i16 {
            return Err(format!("node {v} fired t{t} out of order"));
        }
        n.processed[v] = t as i16;
        n.pend[v].retain(|&p| p != t);
        let value = match v {
            X | Y | Z => t as i32 + 1,
            P => at(&n, X, t),
            B => at(&n, P, t) + 10 * at(&n, Y, t),
            C => at(&n, Z, t) + 1000 * at(&n, B, t),
            _ => unreachable!(),
        };
        n.hist[v].push((t, value));
        for d in outputs(v) {
            if n.quiet[d] == QUIET {
                return Err(format!("push of t{t} into quiet node {d}"));
            }
            push(&mut n, t, d)?;
        }
        out.push(n);
    }
    Ok(())
}

fn serial(q: &[usize]) -> [Vec<(u8, i32)>; NODES] {
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
    for mask in 0..3u32.pow(k) {
        let q: Vec<usize> = (0..k).map(|t| [X, Y, Z][(mask / 3u32.pow(t) % 3) as usize]).collect();
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
