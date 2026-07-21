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
| myko-server | `trace = ["hyphae/trace"]` | **drop it.** It gates no myko source at all — a pure forwarding alias. Repointing it at `hyphae/profiling` would just make it a confusing synonym for myko-server's existing `profiling`, which already reaches `hyphae/profiling` transitively. One name per switch. |
| myko-server | `ci = ["profiling", "trace"]` (Cargo.toml:79) | references myko-server's own `trace`, so dropping that feature means editing the CI umbrella too — one decision, two edit sites |
| myko-server | `inspector = ["hyphae/inspector"]` | **already removed** (myko `afc8c6fc`): the feature, the `_inspector: InspectorServer` field on `CellServer`, the `start_server("myko")` call and its port log are gone, and `tokio`/`serde`/`serde_json` dropped out of myko's lockfile. Nothing to migrate |
| rship | no inspector usage at all | nothing |
| myko source | `hyphae::profiling::pass` / `take_report` in `core/src/server/context.rs` | unchanged (already behind myko's own feature with an `emit()` fallback) |
| rship | `--features profiling,hyphae/profiling` | unchanged (`tracy` forwards `hyphae/profiling`, which survives) |
| rship-server | `trace = ["dep:hyphae", "hyphae/trace"]` (apps/server/Cargo.toml:20) | **hard resolve error** on bump — drop it. Nothing consumes it (zero `cfg(feature = "trace")` in `apps/server/src`). Also strip the now-inert `RSHIP_HYPHA_TRACE_*` env from `Makefile.toml`'s `dev-profile` and its "profiling ⇒ trace ⇒ metrics" comment |

**Verification method — note the gap.** The check "zero downstream uses of any
removed API" was run with a *source* grep, which cannot see a removed **feature
name** referenced from a manifest. rship-server's `trace` feature was caught only
by a human reading the PR. The correct check is both:

```sh
grep -rn 'value_debug\|hyphae::registry\|hyphae::server\|with_metrics' --include='*.rs'   # APIs
grep -rn 'hyphae/\(trace\|inspector\|metrics\)' --include='*.toml'                        # feature names
```

Run against myko and rship, the manifest grep returns exactly two hits: myko
`libs/myko/server/Cargo.toml:70` and rship `apps/server/Cargo.toml:20`. Both are
in the migration table above; hyphae's own workspace is clean.

## Open questions

1. ~~`inspector` moves out.~~ **`inspector` is deleted.** Settled 2026-07-21:
   myko is removing its inspector-server integration, and rship never had one,
   so after that lands nothing consumes it. Extraction was the right call only
   while there was a consumer to serve; with none, a `hyphae-inspector` crate
   would be a maintained artifact with no users — the same burden this exercise
   is removing, relocated rather than retired.

   Deletion removes `src/registry.rs` (154 lines), `src/server.rs` (227 lines),
   15 gated sites across 4 files, the `serde` / `serde_json` / `tokio` /
   `uuid/serde` optional dependencies (hyphae stops depending on tokio at all),
   and the two `DepNode` methods that exist solely to feed the registry —
   `value_debug` (dep_node.rs:64) and `caller` (dep_node.rs:69) — which
   simplifies a public trait every operator implements. Confirmed safe
   downstream: myko never implements `DepNode` nor calls either method, and
   rship has no inspector usage at all, so no impl block breaks.

   **Final feature set: `async`, `scheduler`, `profiling`.** One observability
   feature, and it is the one that only wires up external tooling.

2. ~~`#[inline(never)]` in production (constraint 2).~~ **Settled — measured, see
   below.** The two constraints turn out not to collide at all.

3. **Naming.** `profiling` on merit: the feature's entire job is to make the
   crate legible to external profilers, and that is what the word means. The
   alternative, `observability`, is broader than what it delivers — hyphae emits
   spans and symbol boundaries and nothing else; it does not collect, aggregate,
   or export. (Downstream churn deliberately did *not* drive this choice, or
   `inspector`'s: consumers get fixed after hyphae is right.)

## Measured: where the hot-path cost actually lives

`cargo bench --bench latency -- --quick`, same machine, one run per config:

Measured on an idle machine, alternating runs (`benches/latency.rs --quick`):

| build | single cell set + watch | map chain / 20 |
|---|---|---|
| feature off | 47.3 – 48.1 ns | 305 ns |
| `profiling` (new) | 46.4 – 48.3 ns | 302 ns |
| `trace` (old, removed) | 587.7 ns | 1733 ns |
| `profiling` (old, implied trace) | 595.5 ns | 1743 ns |

The design goal is met: the observability build went from **12.5× baseline to
indistinguishable from baseline**, and from ~0.63 GB of resident registry to
zero per-cell bytes.

**Two corrections to this document's own numbers**, both kept visible rather
than quietly edited, because both were quoted to the downstream repos while the
decision was being made:

1. The pre-implementation "~1%" compared the surviving feature against the
   `trace` build rather than against a feature-off build — the comparison a
   reader would assume. Wrong base.
2. The first post-implementation measurement reported +19% / +12%. That was a
   single run taken while parallel cargo builds were competing for cores.
   Alternating repeats on an idle machine did not reproduce it: the feature-on
   runs straddle the feature-off runs.

The honest statement is that the cost is **below this benchmark's resolution**
(±3% run-to-run, so under ~1.5 ns on a 48 ns operation) — not free, since
`record_fire`'s TLS borrow, the name clone and span creation are real work, but
not measurable here. The conclusion that it is cheap enough to leave on in
production survives all three numbers; the discipline worth keeping is that a
benchmark run under load is not evidence.

### Public API removed (2.0.0)

`hyphae::metrics` + `CellMetrics`; `hyphae::tracing`, `CellTraceSnapshot`,
`hot_traced_cells`, `log_hot_cells`; `Cell::with_metrics`, `Cell::metrics`,
`Cell::on_slow_subscriber`, `Cell::is_backed_up`, `Cell::is_backed_up_threshold`,
`Cell::try_set`, `Cell::try_set_threshold`, `SlowSubscriberAlert`. The
`RSHIP_HYPHA_TRACE_*` env vars become inert.

Verified by grep across myko and rship: **zero downstream uses of any of these**.

### Known wart

`Source::with_name` is now a genuine no-op — its whole body was the `trace` call
and `SourceInner` has no name field, so the name is discarded. Kept for API
symmetry with `Cell::with_name` and documented as such. Either give `SourceInner`
a name field that the span reads, or remove the method; leaving a silently
discarding builder is the worst of the three.

## Field validation (2026-07-21, pre-merge)

The design was verified *cheap* by benchmark and verified to *emit* the right
things by construction — but that is validation by design, not by use. Before
merging, rship drove myko's in-process pprof (499 Hz, 20 s) through a live
server built against this branch, real workload, 126 warmed scenes:

```
 234 samples  hyphae::cell::…<CellMutable>>::notify::
 171 samples  hyphae::cell::…<CellMutable>>::fanout
              write_value present
3015 samples  hyphae::platform::native::reactor::
1132 samples  hyphae::scheduler::run_wave::run_group
 587 samples  hyphae::scheduler::batch
 593 samples  hyphae::source::Source<IntervalTick>>::emit
```

Both load-bearing guarantees hold in the field:

- `notify` and `fanout` resolve as **distinct frames** — the `inline(never)`
  boundaries do what they exist for; no folding.
- Cells carry their **type parameters** in the symbol, so fanout is attributable
  by cell type with no in-process registry. This is the concrete replacement for
  the deleted `hot_cells()`, and it confirms the central bet of this design: the
  attribution the registry provided is recoverable from symbols + a standard
  profiler, at zero resident cost.

Scheduler internals resolve too. No missing boundary, no unattributable span, no
change requested by the consumer. The surviving feature is sufficient as shipped.
