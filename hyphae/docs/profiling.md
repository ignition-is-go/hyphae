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

`profiling` is hyphae's *only* observability feature (`metrics` and `trace` were
removed in 2.0.0). It does three things, and compiles to nothing when off:

- Marks `Cell::notify`, `Cell::write_value`, and `Cell::fanout`
  `#[inline(never)]`, so each stays a distinct frame in a sampled stack.
- Emits a `tracing` span (`hyphae.fanout`) per cell emit, tagged with the cell
  `id` and `name` (set names with `Cell::with_name`). hyphae only *emits* spans;
  the application chooses the subscriber (see Layer 3b).
- Exposes [`hyphae::profiling::pass`] / [`take_report`] for measuring how much
  of a propagation pass is redundant re-fire.

It adds **zero per-cell resident bytes** and no global census. With no
subscriber attached it costs, measured against a feature-off build
(`benches/latency.rs`, `--quick`):

| | feature off | `profiling` on | delta |
|---|---|---|---|
| single cell set + watch | 45.8 ns | 54.6 ns | +19% (+8.8 ns) |
| chain depth / map chain 20 | 286 ns | 321 ns | +12% (+35 ns) |

That is the honest number: ~19% relative on the cheapest possible operation,
which is single-digit nanoseconds absolute, from `record_fire`'s thread-local
borrow, the name clone, and span creation. Cheap enough to leave on in
production — which is the point — but it is not free, and it is not the "~1%"
you get if you mistakenly compare against the old `trace` build instead of
against baseline.

For scale, the feature this replaced (`trace`, removed in 2.0) cost **12.5×** on
the same benchmark — 595 ns and 1743 ns — plus ~0.63 GB of resident registry on
a 21 GB production heap.

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

## Layer 4 — memory: live-cell census and attribution

hyphae used to keep an in-process cell registry (`hot_traced_cells()`,
`log_hot_cells()`, behind the old `trace` feature) to answer "how many cells are
live, and where were they created?". That registry is **gone** as of 2.0.0: on a
live rship heap it held 0.63 GB of a 21 GB steady state (~1.6M cells) and 1.7 GB
at a 41 GB peak, and it was ~100% of the feature's hot-path cost.

Use a heap profiler instead. It attributes *retained bytes* by allocation stack —
strictly more information than the registry gave, at sampled cost and zero
resident per-cell overhead. This is not a downgrade: the 0.63 GB figure above was
itself produced by `jeprof` on a live heap. The standard tool is what found the
leak; the bespoke registry was the thing being measured.

- **jemalloc + jeprof** (what was used in production):
  ```rust
  // in the application, not hyphae
  #[global_allocator]
  static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;
  ```
  ```sh
  export MALLOC_CONF="prof:true,prof_active:true,lg_prof_sample:19,prof_prefix:/tmp/jeprof"
  ./target/profiling/rship <args>
  # dump a heap profile (via jemalloc's prof.dump mallctl or on exit), then:
  jeprof --show_bytes --lines ./target/profiling/rship /tmp/jeprof.*.heap
  jeprof --svg  ./target/profiling/rship /tmp/jeprof.*.heap > heap.svg
  ```
  Live-cell census falls out directly: `CellInner<T>` allocations grouped by the
  `Cell::new` call site, sized in retained bytes.
- **pprof-rs** — in-process sampling with no allocator swap, if jemalloc isn't an
  option. Emits pprof protos consumable by `go tool pprof`.
- Diff two profiles taken minutes apart (`jeprof --base`) to isolate *growth*
  rather than steady-state footprint — that is what identifies a leak.

For "which cells are hot" (rather than "which cells are alive"), use the
`tracing` spans from Layer 3b: they carry cell id and name, so a `tracing-flame`
or Tracy timeline gives exact per-fanout counts and durations, including the
slow-subscriber case the old counters covered.

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
5. **Memory pass — what is retained?** Run under jemalloc with profiling on
   (Layer 4) and diff two `jeprof` dumps taken minutes apart. Growth in
   `CellInner<T>` by creation site is the live-cell census.
