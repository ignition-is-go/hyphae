//! Full-scale timed runs of the rship-scale suite. One process runs one
//! scenario once and prints one JSON line with the commit, the scale and
//! every metric. `scripts/rship_scale.py` repeats runs, stores results and
//! compares them with the checked-in baseline.
//!
//! ```sh
//! cargo bench -p hyphae --features scale-bench --bench rship_scale -- --scen probe --scale 1
//! ```

#[path = "support/rship_scale/mod.rs"]
mod rship_scale;

fn main() {
    rship_scale::cli_main();
}
