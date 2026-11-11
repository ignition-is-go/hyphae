# Session execution and reactive scheduling

## Finding and decision

Independent session threads do not provide independent propagation domains in
Hyphae 3.1.10. A generic session benchmark shows substantially worse output tail
latency with concurrent session threads than with serial execution; isolated
processes recover independent progress. Enabling the existing wave worker pool
does not eliminate this effect. These results justify prototyping explicit
scheduler domains, not committing to a production domain API or a process split.

This is a synthetic mechanism experiment, not a production profile. It cannot
establish which fraction of an application's bottleneck is reactive scheduling,
serialization, persistence, indexing, or socket fanout.

## Generic domain model

The application shape motivating this work is a control-orchestration server:
shared authored definitions feed multiple independent execution sessions. Each
session consumes clocks/device inputs, evaluates binding graphs, produces device
actions, and exposes optional live inspection. Editing and lifecycle changes
alter what runs. Domain commands may return to a central authority.

The reusable model has no application-specific entities or Myko dependency:

| Application concern | Generic benchmark representation | Fidelity |
| --- | --- | --- |
| Session-local input / frame | One input cell per session, monotonic sequence | Closed-loop saturation, one outstanding update |
| Authored configuration | A configuration cell joined with input | Copied per session or genuinely shared in-process |
| Binding graph | Width independent lanes, each with materialized depth | Stable, uniform topology; not lazy operator fusion |
| Computation | Deterministic wrapping integer transform per stage | Tunable CPU cost; not representative media processing |
| Device action / terminal value | Joined vector of all lane outputs | Observed completion and final checksum verification |
| Execution triggers | Source emitting an explicit burst each tick | Source delivery count only; no stateful event subgraph |
| Live monitoring | Extra subscribers reducing the output vector | Observer CPU/fanout, no wire encoding |
| Busy neighbor | Last session has higher stage computation cost | Unequal demand with equal width and tick counts |
| Editing | Configuration changes every N local ticks | Value changes, not topology reconstruction |

`serial` runs session loops successively in one process. `threads` has one thread
per session, with a start barrier after warmup. `processes` starts one subprocess
per session, waits for graph initialization, then releases all children. The
children own independent Hyphae globals. Process mode uses copied configuration;
it intentionally rejects live shared configuration because no IPC replication is
implemented. This is an isolation control, not a distributed-runtime benchmark.

## What the implementation tells us

Evidence was checked against source because the knowledge graph's scheduler
index was stale. Current sources, rather than recovered call-graph edges, support
these findings:

- `hyphae/src/scheduler.rs`: `TICK` is a single process-wide mutex-protected
  `SharedTick`; `TICK_ACTIVE` globally gates deferred propagation. Independent
  graphs can therefore enter the same deferred scheduling domain.
- `batch` uses thread-local nesting for stack reentrancy but global queue/depth
  and drain ownership. Concurrent return does not imply observed settlement.
- `drain` has one owner at a time. It processes height waves and can hand off
  after 64 waves while another batch is open. It releases the queue lock while
  callbacks execute: this is not a lock held around all computation.
- `run_wave` parallelizes sufficiently large same-height groups. The default
  threshold is 256 cells. This is parallelism *within* a shared propagation
  frontier, not independent session progress.
- `hyphae/src/executor.rs`: eligible scheduler waves and join work share a
  dedicated lazy Rayon pool. The default is available parallelism capped at
  four threads; `HYPHAE_WORKER_THREADS=0` disables it. Increasing an application's
  global Rayon pool does not resize this dedicated pool.

The existing `scheduler_contention` benchmark is useful for enqueue pressure but
counts batch submissions. Its comments describe an obsolete wait/Condvar protocol,
and its timed function includes thread creation despite a comment saying
otherwise. The new benchmark leaves it unchanged and measures terminal outputs.

## Measurement contract

Each session submits a tick and waits, with a 30-second deadline, for its joined
output to carry that tick's sequence. It records both batch-return duration and
first coherent output-arrival duration. The latter timestamp is taken by the
subscriber, so it includes reactive propagation but not the producer's Condvar
wake-up delay. Session-loop elapsed time includes producer wake-up and any batch
work performed after output arrival. Extra observers may run after the completion
subscriber; their costs are reflected in subsequent scheduling and loop time,
not necessarily in the same tick's output-arrival timestamp.

A batch can drain another session's work, so these times answer different
questions. `returns_before_output` counts the discrepancy explicitly. Each
session allows only one outstanding tick: completed-tick rates cannot be inflated
by overwriting that session's unobserved input values.

Assertions verify every final lane's sequence, configuration and computed value,
and the exact number of source events. Every observed joined vector is checked
for uniform sequence/configuration; incoherent vectors are counted and make the
run fail. Initial/warmup vectors participate in that check. This does not assert
that a shared configuration write and another thread's input are one transaction.
Evaluation counts are diagnostic and may include work overlapping another
session's measurement boundary. Each stage increments a per-session atomic
counter; that instrumentation adds cost and can contend during intra-session
parallel waves. The harness also adds subscriber synchronization and lane metadata
that a production graph may not carry.

Quantiles use sorted samples and floor((N-1)*p/100). Summaries report the median
of per-session p99s, then the median over repetitions: these are **not pooled
p99s**. Approximate completed-tick rate divides total measured ticks by summed
session-loop time for serial mode, or the longest measured session-loop time for
concurrent modes. Concurrent intervals may start at slightly different times,
especially subprocesses warming up independently, so use it as a scaling signal,
not exact common-window throughput. Parent wall times are also preserved and
explicitly include warmup; process wall time additionally includes child exit.

## Initial measurements

Host: AMD Ryzen 9 7900X3D, 12 cores / 24 logical CPUs; Rust 1.97.1; optimized
benchmark profile. Runs were not CPU-pinned and the host was not reserved.
Three randomized repetitions per case, 400 measured ticks and 40 warmup ticks,
1/2/4 sessions. Two matrices each contain 171 successful cases:

- [Workers disabled, raw JSONL](session-workload-results.jsonl) and
  [summary](session-workload-summary.md).
- [Four workers per process, raw JSONL](session-workload-workers4-results.jsonl)
  and [summary](session-workload-workers4-summary.md).

Both use wave threshold 256. A process case can have four independent worker
pools, so the worker-enabled process comparison does not hold the total worker
budget constant. The workers-disabled matrix is the cleaner isolation control.
The initial matrices precede a formatting-only pass and subprocess-cleanup guard;
neither changed graph evaluation or measurement. Future runner outputs include
source and lockfile hashes as well as source commit/dirty state.

Four-session examples with four configured wave workers:

| Shape | Serial p99 µs | Session-thread p99 µs | Process p99 µs |
| --- | ---: | ---: | ---: |
| Cheap: width 16, depth 8 | 30.0 | 173.6 | 31.8 |
| Deep: width 4, depth 48 | 36.2 | 498.8 | 37.9 |
| Compute: width 16, depth 8, work 1024 | 115.5 | 558.9 | 128.6 |
| Wide: width 256, depth 2 | 777.8 | 4,928.6 | 1,078.7 |
| Cheap sessions beside one expensive session | 28.1 | 655.2 | 32.7 |

The last row uses only the cheap sessions' quantiles. Serial runs do not model
simultaneous arrivals, so their low latency does not demonstrate production
fairness. They provide a per-session cost reference. The isolated-process results
are a more useful control for interference.

With wave workers disabled, cheap four-session approximate rates were 46.7k
completed ticks/s serial, 31.1k with session threads, and 167.1k in processes.
Compute-heavy rates were 9.94k, 8.80k and 36.7k respectively. Cheap-session p99
beside an expensive neighbor was 570 µs in threads versus 32 µs in processes.

Cheap wide graphs also regressed when wave workers were enabled: the serial
width-256 case changed from roughly 274 to 778 µs p99. Threshold and work
amortization matter independently of session isolation. Do not lower the wave
threshold globally based on a compute-heavy case alone.

These experiments demonstrate cross-session interference and recoverable
parallel capacity in these shapes. They do not distinguish mutex acquisition,
frontier bookkeeping, allocation, callback scheduling, cache effects or OS
scheduling as the dominant cost. They do not prove that explicit domains can
match isolated processes at equal memory and thread budgets.

## Design directions to test

| Direction | Potential benefit | Main cost / question |
| --- | --- | --- |
| Tune existing wave threshold/pool | Low implementation cost; helps wide expensive waves | Does not isolate independent graphs or guarantee fairness |
| Explicit propagation domains | Separate queues, batch scope, and progress per execution owner | Cell ownership, composition, lifetime and compatibility |
| One graph-owning worker per domain | Explicit ingress/order and predictable ownership | Mailboxes, completion semantics, backpressure and worker scheduling |
| Infer connected components | Transparent isolation of unrelated graphs | Dynamic joins/switches require safe merges; ownership can change unexpectedly |
| Process isolation | Existing hard boundary and independent schedulers | Replication, restart, memory duplication and transport cost |

Prefer an experimental **explicit domain plus graph-owning worker**. Keep the
legacy global domain as the compatibility default. A domain is a propagation
boundary, not automatically an OS thread or its own thread pool. Schedule
independent domains on one bounded executor, with a turn budget. Preserve
intra-domain wave parallelism only when it amortizes well.

Before implementing an API, specify:

1. Domain membership belongs to cells/sources, not only a thread-local scope:
   timer callbacks and worker-pool callbacks must retain the correct domain.
2. Direct cross-domain dependencies are rejected or require an explicit bridge;
   silently merging worlds would recreate hidden coupling.
3. Shared immutable definitions may be copied. Shared live reports either live
   in an authority domain and bridge outputs, or are intentionally recomputed
   in each execution domain. Measure both costs.
4. A bridge declares latest-value versus ordered-event delivery, capacity,
   overload behavior, sequence/revision and startup readiness.
5. `batch` and any settlement/completion primitive define their domain and
   concurrent-caller guarantees. Do not promise settlement just because a
   closure returned.
6. Construction via `switch_map`, dynamic collection projection, subscriptions,
   destruction and timer cancellation must preserve ownership.
7. Shutdown, callback panic, reentrant command feedback and domain cancellation
   must not strand another domain. A bounded executor must not block every
   worker waiting for work queued to that same executor.
8. Native and WASM behavior remain specified; WASM still benefits from explicit
   turns and budgets even without CPU parallelism.

## Next experiments and acceptance criteria

Before choosing a production direction:

- Run a paced/open-loop trace with offered load, queue depth, dropped/coalesced
  state, ordered event latency, deadline misses and admission-to-output latency.
  The current closed-loop benchmark avoids unbounded backlog and therefore
  cannot measure overload or coordinated-omission-free latency.
- Add skewed branch depths, diamonds with multiple changed roots, event-semantic
  subgraphs, dynamic rewiring, keyed collection joins and scene activation /
  teardown. Current stable uniform lanes do not represent their costs.
- Replay measured graph sizes and input/edit rates from a real application;
  retain dependency-free synthetic cases to explain mechanisms.
- Prototype two explicit domains plus a bounded latest-value/event bridge.
  Compare against this baseline and process controls with an equal total CPU
  worker budget. Profile enqueue/frontier work separately from callbacks.
- Compare shared computation plus bridging against per-domain duplicated
  computation. Memory/RSS, allocations and bridge bytes belong beside CPU and
  latency measurements.

Accept domains only if independent sessions improve throughput and cheap-session
tail latency under a noisy neighbor, single-session overhead stays acceptable,
and bridge/rewiring correctness survives stress. Establish numerical budgets
from application deadlines rather than treating this host's microseconds as an
SLO. Process isolation remains the control, not an assumption that a distributed
architecture wins after adding replication and encoding.

## Reproduce

```sh
python3 tools/bench-session-workload.py --output /tmp/sessions-workers0.jsonl
python3 tools/bench-session-workload.py --output /tmp/sessions-workers4.jsonl --workers 4
python3 tools/summarize-session-workload.py /tmp/sessions-workers4.jsonl

# Focused assertion-backed run; environment is read once per process.
HYPHAE_WORKER_THREADS=0 cargo bench -p hyphae --features scheduler \
  --bench session_workload -- --mode threads --sessions 4 --width 16 \
  --depth 8 --ticks 1000 --warmup 100 --hot-work 4096
```

The runner builds only this benchmark, randomizes case order with a fixed seed,
preserves commands/environment metadata and failures, and gives each case a
120-second timeout. It does not modify runtime configuration outside its child
processes. No scheduler implementation changes are included in this work.

## Validation and tracked follow-ups

All 342 matrix cases passed output/event/coherence assertions. Final harness
validation also passed a 17-case runner smoke sweep, a forced-parallel shared-edit
run (threshold 1, two sessions, event burst 8), and a two-process heterogeneous
run with zero warmup. Focused benchmark Clippy with warnings denied, focused
nightly Rustfmt, Python compilation and invalid-configuration checks passed.
The full workspace runtime test suite was not run: no runtime code changed.

- `lv-79a7`: experimental explicit domains and bounded bridges.
- `lv-35d9`: paced arrivals, topology/event stress and application-shaped traces.
