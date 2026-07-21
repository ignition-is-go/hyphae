# Collapsing hyphae's observability features to one

Status: proposed (2026-07-21). Supersedes the four-feature split.
Motivating incident: rship's production rack OOM-crash-looping a 48G cgroup.

## The ask

Trevor: *"I don't need the tracing server, just the things that let existing
profiling tools connect to it"* — and, separately, collapse profiling/tracing
down to **one** feature.

Two constraints that pull against each other:

1. Do **not** lose the ability to profile and measure memory in production.
   Instrumentation being on in prod is what surfaced the leak. Memory overhead
   is acceptable for that.
2. Do **not** accept leaks, and don't pay a hot-path pessimization that hyphae's
   own docs describe as opt-in.

## Current state (measured, not assumed)

Four features, boundaries muddled: `inspector`, `metrics`, `trace = [metrics]`,
`profiling = [trace, dep:tracing]`.

| feature | per-cell resident cost | hot-path cost | global state |
|---|---|---|---|
| `inspector` | none (one registry `Weak` entry) | none | `REGISTRY` (registry.rs:149); pulls tokio + serde_json |
| `metrics` | 3 `CellInner` fields (24 B) + 2 `ArcSwap` payloads (~64 B) + `Arc<CellMetrics>` (~56 B) | 2 `Instant::now()` + 2 `ArcSwap::load` + 3 atomics per fanout; **2 `Instant` + 1 CAS per subscriber** | none |
| `trace` | caller `String` + `CellTraceEntry` (72 B), **plus** it flips `Cell::new` to attach metrics to *every* cell (cell.rs:1441) | write-locked `DashMap` update per notify; 2 extra mutex locks per subscribe/unsubscribe | 4 statics, 2 env vars |
| `profiling` | none | `record_fire` TLS + `name.lock()` + `Arc<str>` clone + `trace_span!`; `notify`/`write_value`/`fanout` become `#[inline(never)]` | thread-local `PASS` |

Field data (jeprof, live rship heap): the trace registry alone held **0.63 GB of
a 21 GB steady state** (~1.6M live cells) and **1.7 GB at a 41 GB peak**.
`register_cell` inserts unconditionally — the `RSHIP_HYPHA_TRACE_*` env vars gate
only log *cadence*, never registration.

Three structural problems fall out of that table:

- **`trace` is the expensive one, and it reads like the cheap one.** It is the
  whole 0.63 GB, and it is also the switch that decides whether cells carry
  metrics at all. Anyone reaching for "just the lightweight counters" gets the
  maximum bill. (This nearly shipped: myko PR #39 was briefly revised to
  `["scheduler","trace"]` on exactly that misreading.)
- **Cargo features are additive across the graph.** One dependency naming
  `profiling` imposes it on every consumer, with no downstream opt-out. myko's
  workspace did this to all of rship.
- **hyphae is maintaining an in-process observability service** — a global
  census, a sampler, a reporting loop — parallel to tools that already do this
  better.

## Proposal

**One feature: `profiling`.** `metrics` and `trace` cease to exist as feature
names. It keeps exactly the integration points that let external tooling attach:

- `#[inline(never)]` on `notify` / `write_value` / `fanout`, so sampling
  profilers resolve distinct frames instead of one folded symbol.
- `tracing` spans per fanout, tagged with cell id/name — consumed by Tracy,
  `tracing-flame`, or any subscriber the application installs.

Everything below is **deleted**, not moved:

- `TRACE_RECORDS` + `CellTraceEntry` + the whole of `tracing.rs`'s registry
- `hot_cells` / `log_hot_cells` / `CellTraceSnapshot` (and their `lib.rs`
  re-exports at lib.rs:195)
- the per-cell caller `String` from `Location::caller()`
- `register_cell` / `deregister_cell` / `update_name` / `update_subscriber_count`
  / `update_owned_count` / `record_notify`
- per-cell `Arc<CellMetrics>`, its 2 `ArcSwap`s, the slow-subscriber threshold +
  callback, and the per-subscriber `Instant::now()` pair and CAS
- the `RSHIP_HYPHA_TRACE_LOG_EVERY` / `RSHIP_HYPHA_TRACE_TOP_N` env vars

Net effect on a production build with the feature **on**: zero per-cell resident
overhead, zero global census, no per-notify map write.

### Why constraint 1 is still satisfied

The capability is not lost; it moves to the tool that already does it better.

| capability today | replacement |
|---|---|
| live-cell census attributed to creation site | heap profile (jemalloc/jeprof, or pprof). Attributes *retained bytes* by allocation stack — strictly more than the registry gave, at sampled cost and **zero resident per-cell overhead** |
| hot cells by notify time / count | `tracing` spans → `tracing-flame` or Tracy (exact, not sampled); or `samply`/`perf` for wall-clock |
| slow-subscriber detection | span durations in Tracy's timeline |
| dependency graph topology | `inspector` (see open question) |

The decisive evidence: **the 0.63 GB figure was itself produced by jeprof on a
live heap.** The standard tool was what found the leak. The bespoke registry was
not the instrument — it was the thing being measured.

### Semver

Removing feature names and public items (`hot_traced_cells`, `log_hot_cells`,
`CellTraceSnapshot`) is breaking: **2.0.0**.

## Downstream migration

| call site | today | after |
|---|---|---|
| myko workspace dep | `["scheduler"]` (PR #39, landed) | unchanged — already forward-compatible |
| myko-core | `profiling = ["dep:pprof", "hyphae/profiling"]` | unchanged |
| myko-server | `trace = ["hyphae/trace"]` | **dangling** — drop, or point at `hyphae/profiling` |
| myko-server | `inspector = ["hyphae/inspector"]` | see open question |
| myko source | `hyphae::profiling::pass` / `take_report` in `core/src/server/context.rs` | unchanged (already behind myko's own feature with an `emit()` fallback) |
| rship | `--features profiling,hyphae/profiling` | unchanged |

## Open questions

1. **`inspector`'s fate.** It is independent of trace/metrics, costs nothing on
   the hot path, and is genuinely useful for graph debugging — but it is a tokio
   + serde_json in-process server, i.e. precisely the "bespoke service" class
   Trevor wants out. Folding it into the single feature would drag tokio into
   production builds, so that is the one option to reject. Recommend extracting
   it to a `hyphae-inspector` crate (keeps the capability, ends the maintenance
   burden inside hyphae, and leaves hyphae with exactly one feature) — but
   confirm first whether anything actually consumes it today.

2. ~~`#[inline(never)]` in production (constraint 2).~~ **Settled — measured, see
   below.** The two constraints turn out not to collide at all.

3. **Naming.** `profiling` is retained rather than something broader like
   `observability` purely to minimize downstream churn: myko-core and rship
   already name `hyphae/profiling`, so they need no edit at all.

## Measured: where the hot-path cost actually lives

`cargo bench --bench latency -- --quick`, same machine, one run per config:

| build | single cell set + watch | map chain / 20 | vs baseline |
|---|---|---|---|
| `scheduler` (baseline) | 47.8 ns | 301 ns | — |
| `scheduler,metrics` | 47.4 ns | 285 ns | free (within noise) |
| `scheduler,trace` | 587.7 ns | 1733 ns | **12.3× / 5.8×** |
| `scheduler,profiling` | 595.5 ns | 1743 ns | 12.5× / 5.8× |

Three conclusions, and together they dissolve the apparent conflict between
Trevor's two constraints:

- **`trace` is ~100% of the pessimization**, not just ~100% of the memory. The
  registry is a single cause behind both symptoms — and it is the thing being
  deleted.
- **Spans + `#[inline(never)]` cost ~1%** (595 ns vs 588 ns; 1743 ns vs 1733 ns).
  That is the entire surviving feature. It is cheap enough to leave on in
  production, which is exactly what constraint 1 asks for. Caveat: this is with
  no `tracing` subscriber installed — the "on in prod, nobody attached" case.
  Attaching Tracy or `tracing-flame` makes the span cost real, as intended.
- **`metrics` alone is measurably free** — because `trace` is what decides
  whether cells carry metrics at all (cell.rs:1441). As a standalone feature it
  does nothing observable today, which is an independent argument for deleting
  the name.

So the post-collapse feature is ~1% on the hot path and zero per-cell resident
bytes, versus 12.5× and 0.63 GB today. Constraint 2 is satisfied without giving
up anything constraint 1 asked for.
