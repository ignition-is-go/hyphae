# Profiling hyphae-heavy applications

When you flamegraph an application that leans hard on hyphae (rship being the
motivating case), the reactive hot path tends to collapse into a single enormous
frame — often `<Uuid as PartialEq>::eq` or `Cell::notify` — that accounts for
most of the samples and tells you almost nothing about *which* cells or
operators are actually expensive.

Two things cause that:

1. **Inlining.** `notify` → `fanout` → the subscriber closures get inlined into
   one another. A sampling profiler then attributes every sample to whichever
   outer symbol survived, so the whole cascade reads as one frame.
2. **Symbol folding (ICF + generic dedup).** After optimization, many
   monomorphizations of `Cell<T>::notify` have byte-identical bodies. Linker
   identical-code-folding and `-Zshare-generics` collapse them to one symbol, so
   fanouts from *unrelated* cells all report under the same address.

The `profiling` feature plus a few build flags fix both.

## Layer 1 — enable hyphae's `profiling` feature

```toml
hyphae = { version = "...", features = ["profiling"] }
```

This does two things (and compiles to nothing when the feature is off):

- Marks `Cell::notify`, `Cell::write_value`, and `Cell::fanout`
  `#[inline(never)]`, so each stays a distinct frame in a sampled stack.
- Emits a `tracing` span (`hyphae.fanout`) per cell emit, tagged with the cell
  `id` and `name` (set names with `Cell::with_name`). hyphae only *emits* spans;
  the application chooses the subscriber (see Layer 3b).

`profiling` implies `trace`, so the in-process counters (Layer 4) are available
too.

## Layer 2 — build flags that keep stacks readable

Add a dedicated profile to the *application's* `Cargo.toml` (not hyphae's):

```toml
[profile.profiling]
inherits = "release"
debug = "line-tables-only"   # symbols + line numbers, without full-debuginfo bloat
```

Build with `cargo build --profile profiling`. Pair it with `RUSTFLAGS` for
walkable stacks and less cross-cell folding *while profiling*:

```sh
export RUSTFLAGS="-Cforce-frame-pointers=yes -Csymbol-mangling-version=v0"
# nightly only — stop identical generic instantiations folding together:
#   -Zshare-generics=off
```

- `force-frame-pointers=yes` — frame-pointer unwinding, which most samplers walk
  faster and more reliably than DWARF.
- `symbol-mangling-version=v0` — legible demangled names.
- `-Zshare-generics=off` (nightly) — the biggest lever against "every cell is the
  same symbol"; only needed if folding is still hiding distinct cells.

## Layer 3a — sampling profilers

With `profiling` on, `notify` / `fanout` / subscriber frames now separate.

- **samply** (recommended; cross-platform, opens the Firefox Profiler UI):
  ```sh
  samply record ./target/profiling/rship <args>
  ```
- **cargo-flamegraph** (Linux `perf` / macOS dtrace):
  ```sh
  cargo flamegraph --profile profiling --bin rship -- <args>
  ```
- **perf directly**, for control over sample rate and unwinding:
  ```sh
  perf record -g --call-graph=fp -F 4000 ./target/profiling/rship <args>
  perf script | inferno-flamegraph > flame.svg
  ```

## Layer 3b — span-based profiling (structured, deterministic)

The `profiling` feature emits `tracing` spans; attach a subscriber in the app.
This is the better signal for scheduler / frame-lock work: it is not sampled, so
per-fanout timings are exact, and once the scheduler adds a per-frame span the
fanouts will nest under their frame — flush width and per-level parallelism
become directly readable.

- **tracing-flame** — deterministic flamegraph from span enter/exit:
  ```rust
  use tracing_flame::FlameLayer;
  use tracing_subscriber::{prelude::*, registry::Registry};

  let (flame, _guard) = FlameLayer::with_file("tracing.folded").unwrap();
  Registry::default().with(flame).init();
  // ...run the workload, drop _guard to flush...
  // then: inferno-flamegraph < tracing.folded > tracing-flame.svg
  ```
- **tracing-tracy** — live Tracy timeline:
  ```rust
  use tracing_subscriber::prelude::*;
  tracing_subscriber::registry()
      .with(tracing_tracy::TracyLayer::default())
      .init();
  ```

If the application is span-heavy elsewhere, scope to hyphae with an
`EnvFilter` (e.g. `hyphae=trace`) so the flamegraph isn't drowned out.

## Layer 4 — in-process counters (no external tooling)

The `trace` feature (implied by `profiling`) keeps lightweight per-cell counters
you can read from inside the process:

- `hyphae::hot_traced_cells()` — a snapshot of the cells with the most notifies /
  subscribers.
- `hyphae::log_hot_cells()` — logs that snapshot via `log`.

Good for a first "which nodes are hot?" pass before reaching for a profiler.

## Recommended recipe for rship

1. Forward the feature: a `profiling` cargo feature on rship that enables
   `hyphae/profiling`.
2. Add `[profile.profiling]` (Layer 2) and build `--profile profiling`.
3. **First pass — where does wall time go?** `samply record` the real binary
   with real symbols; find the hot operators/cells.
4. **Focused pass — exact per-frame cost.** Attach `tracing-flame`, run one
   representative frame-storm, and `inferno-flamegraph` the folded output. Track
   *fanout time per frame* across scheduler phases; in the frame-lock phase this
   is joined by the emit-vs-boundary jitter histogram as the headline metric.
