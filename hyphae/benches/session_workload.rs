//! Generic independent-session graphs; time observed completion, not submission.
use std::{
    env,
    hint::black_box,
    io::{BufRead, BufReader, Read, Write},
    process::{Command, Stdio},
    sync::{
        Arc, Barrier, Condvar, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use hyphae::{
    Cell, CellImmutable, CellMutable, Gettable, JoinExt, MapExt, Materialize, Mutable, Signal,
    Source, SubscriptionGuard, Watchable, batch, join_vec,
};
use serde_json::{Value, json};

#[derive(Clone)]
struct Config {
    mode: String,
    sharing: String,
    sessions: usize,
    width: usize,
    depth: usize,
    work: u64,
    ticks: u64,
    warmup: u64,
    edit_every: u64,
    observers: usize,
    event_burst: u64,
    child: bool,
    hot_work: u64,
}

impl Config {
    fn parse() -> Result<Self, String> {
        let mut cfg = Self {
            mode: "threads".into(),
            sharing: "copied".into(),
            sessions: 4,
            width: 16,
            depth: 8,
            work: 0,
            ticks: 1000,
            warmup: 100,
            edit_every: 0,
            observers: 0,
            event_burst: 1,
            child: false,
            hot_work: 0,
        };
        let mut args = env::args().skip(1);
        while let Some(arg) = args.next() {
            if arg == "--bench" {
                continue;
            }
            if arg == "--child" {
                cfg.child = true;
                continue;
            }
            if arg == "--help" {
                println!(
                    "session_workload --mode serial|threads|processes --sharing copied|shared --sessions N --width N --depth N --work N --ticks N --warmup N --edit-every N --observers N --event-burst N --hot-work N"
                );
                return Err("help requested".into());
            }
            let value = args
                .next()
                .ok_or_else(|| format!("missing value for {arg}"))?;
            let integer = || {
                value
                    .parse::<u64>()
                    .map_err(|_| format!("invalid integer for {arg}"))
            };
            let size = || usize::try_from(integer()?).map_err(|_| format!("{arg} too large"));
            match arg.as_str() {
                "--mode" => cfg.mode = value,
                "--sharing" => cfg.sharing = value,
                "--sessions" => cfg.sessions = size()?,
                "--width" => cfg.width = size()?,
                "--depth" => cfg.depth = size()?,
                "--work" => cfg.work = integer()?,
                "--ticks" => cfg.ticks = integer()?,
                "--warmup" => cfg.warmup = integer()?,
                "--edit-every" => cfg.edit_every = integer()?,
                "--observers" => cfg.observers = size()?,
                "--event-burst" => cfg.event_burst = integer()?,
                "--hot-work" => cfg.hot_work = integer()?,
                _ => return Err(format!("unknown argument {arg}")),
            }
        }
        if !matches!(cfg.mode.as_str(), "serial" | "threads" | "processes")
            || !matches!(cfg.sharing.as_str(), "copied" | "shared")
            || cfg.sessions == 0
            || cfg.width == 0
            || cfg.depth == 0
            || cfg.ticks == 0
        {
            return Err("invalid mode/sharing or zero sessions/width/depth/ticks".into());
        }
        if cfg.mode == "processes" && cfg.sharing == "shared" {
            return Err(
                "processes require copied configuration; live replication is not modeled".into(),
            );
        }
        Ok(cfg)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Lane {
    sequence: u64,
    revision: u64,
    value: u64,
}

fn compute(mut value: u64, work: u64) -> u64 {
    for i in 0..work {
        value = black_box(value.wrapping_mul(31).wrapping_add(i));
    }
    value.wrapping_add(1)
}

#[derive(Default)]
struct Completion {
    sequence: u64,
    at: Option<Instant>,
}

struct Session {
    input: Cell<u64, CellMutable>,
    config: Cell<u64, CellMutable>,
    output: Cell<Vec<Lane>, CellImmutable>,
    events: Source<u64>,
    event_count: Arc<AtomicU64>,
    completion: Arc<(Mutex<Completion>, Condvar)>,
    incoherent: Arc<AtomicU64>,
    evaluations: Arc<AtomicU64>,
    _guards: Vec<SubscriptionGuard>,
}

impl Session {
    fn build(cfg: &Config, config: Cell<u64, CellMutable>) -> Self {
        let input = Cell::new(0_u64);
        let base = input
            .clone()
            .lock()
            .join(config.clone().lock())
            .materialize();
        let evaluations = Arc::new(AtomicU64::new(0));
        let lanes = (0..cfg.width)
            .map(|offset| {
                let counter = evaluations.clone();
                let offset = u64::try_from(offset).unwrap_or(u64::MAX);
                let work = cfg.work;
                let mut tail = base
                    .clone()
                    .map(move |(sequence, revision)| {
                        counter.fetch_add(1, Ordering::Relaxed);
                        Lane {
                            sequence: *sequence,
                            revision: *revision,
                            value: compute(
                                sequence.wrapping_add(*revision).wrapping_add(offset),
                                work,
                            ),
                        }
                    })
                    .materialize();
                for _ in 1..cfg.depth {
                    let counter = evaluations.clone();
                    tail = tail
                        .map(move |lane| {
                            counter.fetch_add(1, Ordering::Relaxed);
                            Lane {
                                value: compute(lane.value, work),
                                ..lane.clone()
                            }
                        })
                        .materialize();
                }
                tail
            })
            .collect();
        let output = join_vec(lanes).materialize();
        let completion = Arc::new((Mutex::new(Completion::default()), Condvar::new()));
        let incoherent = Arc::new(AtomicU64::new(0));
        let signal_completion = completion.clone();
        let bad = incoherent.clone();
        let mut guards = vec![output.subscribe(move |signal| {
            if let Signal::Value(values) = signal {
                let Some(first) = values.first() else {
                    return;
                };
                if !values
                    .iter()
                    .all(|lane| lane.sequence == first.sequence && lane.revision == first.revision)
                {
                    bad.fetch_add(1, Ordering::Relaxed);
                    return;
                }
                let (lock, ready) = &*signal_completion;
                let mut done = lock
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if first.sequence > done.sequence {
                    done.sequence = first.sequence;
                    done.at = Some(Instant::now());
                    drop(done);
                    ready.notify_one();
                }
            }
        })];
        for _ in 0..cfg.observers {
            guards.push(output.subscribe(|signal| {
                if let Signal::Value(values) = signal {
                    black_box(
                        values
                            .iter()
                            .fold(0_u64, |sum, lane| sum.wrapping_add(lane.value)),
                    );
                }
            }));
        }
        let events = Source::new();
        let event_count = Arc::new(AtomicU64::new(0));
        let counter = event_count.clone();
        guards.push(events.subscribe(move |signal| {
            if matches!(signal, Signal::Value(_)) {
                counter.fetch_add(1, Ordering::Relaxed);
            }
        }));
        Self {
            input,
            config,
            output,
            events,
            event_count,
            completion,
            incoherent,
            evaluations,
            _guards: guards,
        }
    }

    fn tick(&self, cfg: &Config, sequence: u64) -> Result<(u128, u128), String> {
        let start = Instant::now();
        batch(|| {
            if cfg.edit_every != 0 && sequence.is_multiple_of(cfg.edit_every) {
                self.config.set(sequence);
            }
            self.input.set(sequence);
            for event in 0..cfg.event_burst {
                self.events.emit(event);
            }
        });
        let submitted = start.elapsed().as_nanos();
        let (lock, ready) = &*self.completion;
        let mut done = lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let deadline = start
            .checked_add(Duration::from_secs(30))
            .ok_or("deadline overflow")?;
        while done.sequence < sequence {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(format!("output timed out at sequence {sequence}"));
            }
            let (next, _) = ready
                .wait_timeout(done, remaining)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            done = next;
        }
        let completed = done
            .at
            .ok_or("missing completion timestamp")?
            .saturating_duration_since(start)
            .as_nanos();
        drop(done);
        Ok((submitted, completed))
    }

    fn verify(&self, cfg: &Config) {
        let sequence = cfg.warmup.saturating_add(cfg.ticks);
        let revision = self.config.get();
        let values = self.output.get();
        assert_eq!(values.len(), cfg.width);
        for (offset, lane) in values.iter().enumerate() {
            let offset = u64::try_from(offset).unwrap_or(u64::MAX);
            let mut expected = sequence.wrapping_add(revision).wrapping_add(offset);
            for _ in 0..cfg.depth {
                expected = compute(expected, cfg.work);
            }
            assert_eq!(
                lane,
                &Lane {
                    sequence,
                    revision,
                    value: expected
                }
            );
        }
        assert_eq!(
            self.event_count.load(Ordering::Relaxed),
            sequence.saturating_mul(cfg.event_burst)
        );
    }
}

fn quantiles(samples: &mut [u128]) -> Value {
    samples.sort_unstable();
    let at = |percent: usize| {
        samples
            .get(
                samples
                    .len()
                    .saturating_sub(1)
                    .saturating_mul(percent)
                    .checked_div(100)
                    .unwrap_or(0),
            )
            .copied()
            .unwrap_or(0)
    };
    json!({"p50": at(50), "p95": at(95), "p99": at(99), "max": samples.last()})
}

fn run_session(
    session: &Session,
    cfg: &Config,
    barrier: Option<&Barrier>,
) -> Result<Value, String> {
    for tick in 1..=cfg.warmup {
        session.tick(cfg, tick)?;
    }
    if let Some(barrier) = barrier {
        barrier.wait();
    }
    let baseline = session.evaluations.load(Ordering::Relaxed);
    let start = Instant::now();
    let mut submitted = Vec::new();
    let mut completed = Vec::new();
    let mut early_returns = 0_u64;
    for tick in 1..=cfg.ticks {
        let (submission, completion) = session.tick(cfg, cfg.warmup.saturating_add(tick))?;
        if submission < completion {
            early_returns = early_returns.saturating_add(1);
        }
        submitted.push(submission);
        completed.push(completion);
    }
    let elapsed = start.elapsed().as_nanos();
    Ok(
        json!({"kind": "session", "elapsed_ns": elapsed, "ticks": cfg.ticks,
        "batch_return_ns": quantiles(&mut submitted), "output_latency_ns": quantiles(&mut completed),
        "returns_before_output": early_returns,
        "work": cfg.work,
        "evaluations": session.evaluations.load(Ordering::Relaxed).saturating_sub(baseline)}),
    )
}

#[allow(
    clippy::needless_collect,
    reason = "spawn every worker before joining: lazy joins deadlock the start barrier"
)]
fn run_local(cfg: &Config) -> Result<(), String> {
    let shared = Cell::new(0_u64);
    let setup = Instant::now();
    let configs: Vec<_> = (0..cfg.sessions)
        .map(|index| {
            let mut session_cfg = cfg.clone();
            if cfg.hot_work != 0 && index == cfg.sessions.saturating_sub(1) {
                session_cfg.work = cfg.hot_work;
            }
            session_cfg
        })
        .collect();
    let sessions: Vec<_> = configs
        .iter()
        .map(|session_cfg| {
            Session::build(
                session_cfg,
                if cfg.sharing == "shared" {
                    shared.clone()
                } else {
                    Cell::new(0_u64)
                },
            )
        })
        .collect();
    let setup_ns = setup.elapsed().as_nanos();
    if cfg.child {
        println!("READY");
        std::io::stdout().flush().map_err(|e| e.to_string())?;
        let mut line = String::new();
        std::io::stdin()
            .read_line(&mut line)
            .map_err(|e| e.to_string())?;
        if line.trim() != "GO" {
            return Err("missing process start signal".into());
        }
    }
    let start = Instant::now();
    let results = if cfg.mode == "serial" {
        sessions
            .iter()
            .zip(&configs)
            .map(|(session, session_cfg)| run_session(session, session_cfg, None))
            .collect::<Result<Vec<_>, _>>()?
    } else {
        thread::scope(|scope| {
            let barrier = Arc::new(Barrier::new(cfg.sessions));
            let handles: Vec<_> = sessions
                .iter()
                .zip(&configs)
                .map(|(session, session_cfg)| {
                    let barrier = barrier.clone();
                    scope.spawn(move || run_session(session, session_cfg, Some(&barrier)))
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().map_err(|_| "worker panicked".to_owned())?)
                .collect::<Result<Vec<_>, String>>()
        })?
    };
    let elapsed_ns = start.elapsed().as_nanos();
    // All producers are finished here; use a final barrier tick before checking every lane.
    batch(|| {});
    for (session, session_cfg) in sessions.iter().zip(&configs) {
        session.verify(session_cfg);
    }
    let incoherent: u64 = sessions
        .iter()
        .map(|s| s.incoherent.load(Ordering::Relaxed))
        .sum();
    println!(
        "{}",
        json!({"kind": "run", "mode": cfg.mode, "sharing": cfg.sharing,
        "sessions": cfg.sessions, "width": cfg.width, "depth": cfg.depth, "work": cfg.work,
        "ticks": cfg.ticks, "warmup": cfg.warmup, "edit_every": cfg.edit_every,
        "hot_work": cfg.hot_work, "observers": cfg.observers, "event_burst": cfg.event_burst,
        "worker_threads_env": env::var("HYPHAE_WORKER_THREADS").ok(),
        "wave_threads_env": env::var("HYPHAE_WAVE_THREADS").ok(),
        "wave_threshold_env": env::var("HYPHAE_WAVE_THRESHOLD").ok(),
        "setup_ns": setup_ns, "wall_ns_including_warmup": elapsed_ns,
        "incoherent_outputs": incoherent, "results": results})
    );
    if incoherent != 0 {
        return Err(format!("observed {incoherent} incoherent graph outputs"));
    }
    Ok(())
}

struct ChildProcess(std::process::Child);

impl Drop for ChildProcess {
    fn drop(&mut self) {
        // Failed setup or sibling failures must not leave a waiting child behind.
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn run_processes(cfg: &Config) -> Result<(), String> {
    let executable = env::current_exe().map_err(|e| e.to_string())?;
    let mut children = Vec::new();
    for index in 0..cfg.sessions {
        let work = if cfg.hot_work != 0 && index == cfg.sessions.saturating_sub(1) {
            cfg.hot_work
        } else {
            cfg.work
        };
        let mut child = ChildProcess(
            Command::new(&executable)
                .args(["--mode", "serial", "--sessions", "1", "--child"])
                .args([
                    "--width",
                    &cfg.width.to_string(),
                    "--depth",
                    &cfg.depth.to_string(),
                    "--work",
                    &work.to_string(),
                    "--ticks",
                    &cfg.ticks.to_string(),
                    "--warmup",
                    &cfg.warmup.to_string(),
                    "--edit-every",
                    &cfg.edit_every.to_string(),
                    "--observers",
                    &cfg.observers.to_string(),
                    "--event-burst",
                    &cfg.event_burst.to_string(),
                ])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .map_err(|e| e.to_string())?,
        );
        let mut output = BufReader::new(child.0.stdout.take().ok_or("missing stdout")?);
        let mut ready = String::new();
        output.read_line(&mut ready).map_err(|e| e.to_string())?;
        if ready.trim() != "READY" {
            return Err(format!("child did not initialize: {ready}"));
        }
        children.push((child, output));
    }
    let start = Instant::now();
    for (child, _) in &mut children {
        child
            .0
            .stdin
            .take()
            .ok_or("missing stdin")?
            .write_all(b"GO\n")
            .map_err(|e| e.to_string())?;
    }
    let mut results = Vec::new();
    for (mut child, mut output) in children {
        let mut data = String::new();
        output
            .read_to_string(&mut data)
            .map_err(|e| e.to_string())?;
        let status = child.0.wait().map_err(|e| e.to_string())?;
        if !status.success() {
            return Err(format!("child failed: {status}"));
        }
        results.push(serde_json::from_str::<Value>(data.trim()).map_err(|e| e.to_string())?);
    }
    println!(
        "{}",
        json!({"kind": "process_run", "sessions": cfg.sessions,
        "wall_ns_including_warmup_and_exit": start.elapsed().as_nanos(), "results": results})
    );
    Ok(())
}

fn main() -> Result<(), String> {
    let cfg = match Config::parse() {
        Ok(cfg) => cfg,
        Err(message) if message == "help requested" => return Ok(()),
        Err(message) => return Err(message),
    };
    if cfg.mode == "processes" {
        run_processes(&cfg)
    } else {
        run_local(&cfg)
    }
}
