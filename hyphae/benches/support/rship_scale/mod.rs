//! The rship-scale suite: level 1 of the validation ladder.
//!
//! One graph shaped like the rship server at load-test scale, and scenarios
//! that each reproduce a load that broke an earlier scheduler design. The
//! bench target `rship_scale` runs one scenario at full scale and prints one
//! JSON line. The test target `rship_scale` runs every scenario at reduced
//! scale and asserts only the correctness counts. `docs/benchmarks/rship-scale.md`
//! has the scenario list, the gates, and how to compare against the baseline.

// Measurement code: counts, indices and nanosecond arithmetic on values the
// suite itself produced. The workspace lints exist to keep panics and silent
// wraparound out of the library, and a wrong index here fails the run.
#![allow(
    clippy::pedantic,
    clippy::nursery,
    clippy::arithmetic_side_effects,
    clippy::as_conversions,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::exit,
    dead_code
)]

// The one line a branch changes when it changes how a write is submitted.
// `adapter_settle_core.rs` and `adapter_h4.rs` hold the adapters for the
// `hyphae-settle-core` and `h4` branches.
#[path = "adapter_v3.rs"]
pub mod adapter;
pub mod dag;
pub mod load;
pub mod scen_cost;
pub mod scen_solo;
pub mod scen_tick;
pub mod universe;
pub mod util;

use universe::Sizes;
use util::Metrics;

pub const SCENARIOS: [&str; 17] = [
    "tick", "probe", "shared", "pulse", "burst", "edit", "churn", "setup", "overload", "fanout",
    "hotcone", "feedback", "writers", "diamond", "deep", "demand", "oracle",
];

pub struct Ctx {
    pub scale: usize,
    pub workers: usize,
    /// Measured ticks after the warm-up.
    pub ticks: u64,
    pub warm: u64,
    /// Total writes for the oracle scenario.
    pub writes: u64,
    /// Divides every fixed phase length. 1 for a timed run.
    pub time_div: u64,
    pub sizes: Sizes,
    /// Load the store one row write at a time. See `scen_tick::build`.
    pub row_by_row: bool,
    pub m: Metrics,
}

impl Ctx {
    /// A phase of `ms` milliseconds at full length, in milliseconds.
    pub fn ms(&self, ms: u64) -> u64 {
        (ms / self.time_div).max(1)
    }

    /// The same phase in nanoseconds.
    pub fn ns(&self, ms: u64) -> u64 {
        self.ms(ms) * 1_000_000
    }

    /// The reduced-scale configuration the tests use: 1/40 of every part,
    /// 30 ticks, phases a tenth as long.
    pub fn reduced() -> Self {
        Self {
            scale: 1,
            workers: 4,
            ticks: 30,
            warm: 5,
            writes: 40_000,
            time_div: 10,
            sizes: Sizes::new(1, 1.0, 40.0),
            row_by_row: true,
            m: Metrics::default(),
        }
    }
}

/// Run one scenario. False when the name is unknown.
pub fn run(scen: &str, ctx: &mut Ctx) -> bool {
    match scen {
        "tick" => scen_tick::tick(ctx),
        "probe" => scen_tick::probe(ctx),
        "shared" => scen_tick::shared(ctx),
        "pulse" => scen_tick::pulse(ctx),
        "burst" => scen_tick::burst(ctx),
        "edit" => scen_tick::edit(ctx),
        "churn" => scen_tick::churn(ctx),
        "setup" => scen_tick::setup(ctx),
        "overload" => scen_tick::overload(ctx),
        "fanout" => scen_tick::fanout(ctx),
        "hotcone" => scen_tick::hotcone(ctx),
        "feedback" => scen_solo::feedback(ctx),
        "writers" => scen_solo::writers(ctx),
        "diamond" => scen_solo::diamond(ctx),
        "deep" => scen_solo::deep(ctx),
        "demand" => scen_solo::demand(ctx),
        "oracle" => scen_solo::oracle(ctx),
        "selftest" => scen_solo::selftest(ctx),
        "cost" => scen_cost::cost(ctx),
        "diamonds" => scen_cost::diamonds_dedup(ctx),
        "collection_reads" => scen_cost::backport(ctx),
        _ => return false,
    }
    true
}

fn arg<T: std::str::FromStr>(name: &str, default: T) -> T {
    let args: Vec<String> = std::env::args().collect();
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// The commit of the checkout the bench was built from, with `-dirty` when
/// the tree has changes. `HYPHAE_BENCH_COMMIT` overrides it.
fn commit() -> String {
    if let Ok(commit) = std::env::var("HYPHAE_BENCH_COMMIT") {
        return commit;
    }
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .output()
            .ok()
            .filter(|out| out.status.success())
            .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    let Some(head) = git(&["rev-parse", "--short=12", "HEAD"]) else {
        return "unknown".into();
    };
    let dirty =
        git(&["status", "--porcelain", "--untracked-files=no"]).is_some_and(|s| !s.is_empty());
    if dirty { format!("{head}-dirty") } else { head }
}

/// The bench target's `main`: one scenario, one JSON line on stdout.
pub fn cli_main() {
    let workers = arg("--workers", 4usize);
    adapter::init(workers);
    util::init_clock();
    let scen: String = arg("--scen", "tick".to_string());
    let scale = arg("--scale", 1usize);
    let bg = arg("--bg", 1.0f64);
    let shrink = arg("--shrink", 1.0f64);
    let mut ctx = Ctx {
        scale,
        workers,
        ticks: arg("--ticks", 150u64),
        warm: 15,
        writes: arg("--writes", 1_000_000u64),
        time_div: 1,
        sizes: Sizes::new(scale, bg, shrink),
        row_by_row: scen == "tick" && arg("--rows", "each".to_string()) == "each",
        m: Metrics::default(),
    };
    let other = util::other_cores_busy();
    let name = scen.clone();
    // Scenario threads get the stack a server thread has.
    let joined = std::thread::Builder::new()
        .stack_size(8 << 20)
        .spawn(move || {
            if !run(&name, &mut ctx) {
                eprintln!("unknown scenario {name}");
                std::process::exit(2);
            }
            ctx
        })
        .expect("spawn scenario thread")
        .join();
    let Ok(mut ctx) = joined else {
        // The panic message is on stderr.
        std::process::exit(3);
    };
    ctx.m.set("rss_end.mb", util::rss_bytes() as f64 / 1e6);
    println!(
        "{{\"suite\":\"rship_scale\",\"commit\":\"{}\",\"engine\":\"{}\",\"scen\":\"{scen}\",\"scale\":{scale},\"bg\":{bg},\"shrink\":{shrink},\"workers\":{workers},\"other_cores\":{other:.2},\"cells\":{},\"subscribers\":{},\"metrics\":{}}}",
        commit(),
        adapter::NAME,
        ctx.sizes.cells(),
        ctx.sizes.subscribers(),
        ctx.m.json()
    );
    // Tearing down 250,000 cells is not part of any scenario.
    std::process::exit(0);
}

static ONE_AT_A_TIME: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

/// Run `scen` at reduced scale and return every correctness count that is
/// not 0. Scenarios share process-wide state, so runs are serialized.
pub fn violations(scen: &str) -> Vec<(String, f64)> {
    let _guard = ONE_AT_A_TIME.lock();
    adapter::init(4);
    util::init_clock();
    let mut ctx = Ctx::reduced();
    assert!(run(scen, &mut ctx), "unknown scenario {scen}");
    ctx.m
        .0
        .into_iter()
        .filter(|(name, value)| name.starts_with("viol.") && *value != 0.0)
        .collect()
}

/// Run the self-test and return its counts by name.
pub fn selftest() -> std::collections::BTreeMap<String, f64> {
    let _guard = ONE_AT_A_TIME.lock();
    adapter::init(4);
    util::init_clock();
    let mut ctx = Ctx::reduced();
    scen_solo::selftest(&mut ctx);
    ctx.m.0
}
