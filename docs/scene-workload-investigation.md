# Production-scale scene benchmark

## Purpose

Compare the current Hyphae evaluator with the Rust port of Jane Street
Incremental on the same generic computation at an observed application's graph
scale. This is a performance and semantic-adapter experiment, not a replay of
production application code or an endorsement of a replacement library.

## Read-only inventory

The source was the connected MCP endpoint
`myko.rship.hrlv-dev.pulse.rocks/myko/mcp`, server version
`4.0.0-canary.151`, read on 2026-11-11 around 21:52 UTC. All operations were
queries/reports; no commands, scene activation, executor actions or configuration
writes were issued. The Project entity had no saved agent context.

The selected production session had 28 active scene rows at capture time. Its
connected `HRLV_Content` executor cluster was assigned to that session. Twelve
available service instances had client IDs present in the current client list.
Persisted instances and assignment rows were not counted as connected clients.
Another production session also had 28 active rows, and other sessions had
additional active rows. The benchmark models one selected session, not all work
performed by the server across every session.

| Observed dimension | Count |
| --- | ---: |
| Active scenes in selected session | 28 |
| Authored nodes | 1,620 |
| Group/comment nodes excluded from computation | 122 |
| Computational authored nodes | 1,498 |
| Explicit connections | 2,204 |
| Resolved scene-output references within active set | 62 |
| Scene-output references outside active set | 3 |
| Property nodes | 245 |
| Comp-element nodes | 267 |
| Explicit switch nodes | 33 |
| Shared control identities / reader nodes | 67 / 74 |
| Referenced comp engines | 9 |
| Connected service executor instances | 12 |

The full service catalog also held 8,067 emitters, 9,164 actions, and 40 comp-engine
contracts. These registered counts include retained/validation contracts: they
are not counts of active properties, active engines, or changing values.

The scene-size distribution is deliberately uneven: the largest contains 605
nodes, the next 204, and others range down to two nodes. The explicit dependency
DAG including resolved scene-output references has a longest path of 18 edges.
On computational nodes it has 238 weak components; the largest has 800 nodes.
Adding shared-control identity connections produces 237 components. These are
**authored graph relationships**, not an inventory of Hyphae's runtime cells:
shared report/schema dependencies, clocks, blend/variable relationships and
external effects can connect them further.

The [fixture](../hyphae/benches/fixtures/scene_topology.json) preserves node kinds,
scene membership, edge multiplicity, per-node authored-field counts, scene-output
references and selected shared-reference groups. Node/scene IDs were replaced
with indexes. Names, authored values, executor addresses and program payloads
are not included. Capture reads were sequential and non-transactional; they are
not a mutually consistent database snapshot. The topology is validated as an
acyclic graph with valid endpoints before evaluation.

`tools/capture-scene-workload.js` provides a read-only helper for future captures:
append `return await captureSceneWorkload(myko, sessionId, serviceId)` in an MCP
execute script after supplying the intended IDs. Review the selected project's
context and discover operations before using it on another server. Value reads
are bounded to 64 node IDs per query. The original broad value/catalog reads
exceeded the MCP script memory limit; no complete raw entity dump was saved.
The capture helper produces the fixture shape, but the current evaluator's
28-scene maximum is specific to this captured fixture; adapting to another scene
count requires changing that bound as well.

## Generic model and explicit assumptions

The target rate is **60 Hz**, supplied by the user. Production update rates were
not measured by the inventory. Independent executor feedback phases are modeled,
not inferred from retained pulse values.

- `--scenes 10`, `20`, or `28`: choose the largest captured scenes, preserving
  their internal edges. References to omitted publishers become synthetic
  clock-driven external inputs. The output reports how many references were
  excluded. The 10/20 cases are heavy-scene subsets, not a replay of a different
  production activation set.
- Each operator uses deterministic `u64` arithmetic and one scalar output.
  Multiple authored anchors are collapsed into scalar dependencies, preserving
  edge multiplicity. This does not reproduce calc expressions, vector/gradient
  processing, schema logic, comp placement/reconciliation or device behavior.
- One common clock drives clock/speed/wave/sampler roles. One control changes
  per logical frame, round-robin among the retained shared controls. Remaining
  external-source roles consume executor feedback. This is a declared workload,
  not measured source activity. Constants remain constant.
- `--layers 4` materializes four transforms per computational node; 16 is a
  sensitivity case. These represent possible report/operator expansion, not
  measured per-node cell counts.
- Property/action sinks each contribute one output field. In the authored-field
  proxy, comp elements contribute max(1, authored-field-count minus two)
  projections. This yields 3,343 comp field projections plus 247 property/action
  fields for the full fixture: 3,590 output scalars and 5,088 logical plan nodes
  before materialized-layer expansion. Many real authored fields are constant
  or metadata; the proxy can overstate changing field work. `--fields one` is a
  lower-expansion control.
- Explicit switch nodes choose among dependency inputs using a per-scene
  revision. `select` reconnects an existing branch. `rebuild` constructs a fresh
  four-map branch and retires the old one. This exercises actual Hyphae
  `switch_map` and Incremental `bind`, not merely a scalar conditional.
- `--dynamic-sinks on` additionally reconstructs a one-map identity subgraph for
  each property/comp/action sink on scene revision changes, modeling additional
  dynamic query/report boundaries. Their occurrence is an assumption. The
  switch-storm case revises all scenes every logical frame; this is stress,
  not an observed editing rate. The normal case revises them every 60 frames.
- `--feedback staggered` offers one clock/control update and one update from
  each executor per frame. Twelve clients at 60 Hz therefore offer 780 input
  updates/s; fourteen offer 900/s. `batched` updates all roots together at 60 Hz
  as a coalescing control. Every admitted update is evaluated in order.
- Changed scalar outputs are delta-encoded to JSON and fanned out to 12 or 14
  simulated clients. `shared` encodes once and clones an `Arc`; `per-client`
  serializes separately with a client envelope. The client loop consumes bytes
  with `black_box`; it does not open sockets, perform TLS/compression, emulate
  executor application, or model slow-client backpressure. All changed fields are
  broadcast to every simulated client; real service routing, assignment, spatial
  filtering and mixed-service outputs in the captured scenes are not reproduced.
  Output delivery counts
  are simulation accounting, not network acknowledgements.

There is no trigger/event-state-machine implementation, scene lifecycle gate,
keyed relational query engine or automatic graph partitioner in this fixture.
Switching reconstructs branch subgraphs but is not a full scene activation test.
Differential Dataflow is not included in this scalar-cell comparison; it needs a
separate collection/query fixture.

## Execution and fairness

Both adapters construct and evaluate the same generic plan on one dedicated
native evaluator thread. A Tokio current-thread runtime offers paced inputs into
a bounded queue of 64 entries. It never executes the scene graph on a Tokio
worker. Hyphae is compared with its dedicated wave pool disabled and with four
workers, at threshold 256. Incremental 0.2.8 is driven by `stabilise`; its
single-threaded adapter uses one state. This compares existing library behavior,
including demand tracking, equality cutoffs and graph bookkeeping; it does not
attribute an observed difference to one mechanism.

Plan construction and initial output checks precede the measurement interval.
There is no timed warmup; cold first updates and lazy worker-pool creation remain
in the samples. Setup and destruction are reported separately. Every admitted
update checks all output scalars against a simple full-recomputation reference
by default. The reference evaluator does not use either reactive library and
performs no memoization. A final reference check is unconditional. Both backends
produced identical fanout byte counts for the same non-dropping smoke workloads.

The oracle runs after the recorded fanout-completion timestamp, so it is excluded
from that input's evaluation and serialization time. Its CPU still delays later
queued inputs, and is reported separately. `--verify-every N` can sample it for
longer performance experiments while retaining a final full check. Output pulls,
delta scanning and encoding remain real work in the harness.

Paced mode retains the original scheduled arrival time, even when the producer
wakes late. It does not reschedule arrivals after completion. Full queues reject
inputs explicitly; offered/admitted/rejected counts and final evaluation/fanout
accounting are checked. No lossless-event guarantee is claimed for rejected
inputs. Accepted state evolves from accepted inputs only.

Reported metrics include evaluation, switch-update evaluation, delta/encoding,
producer lateness, queue wait, scheduled-arrival-to-simulated-fanout latency,
16.67 ms frame-budget misses at 60 Hz, rejection counts, oracle cost, setup,
teardown, CPU seconds and peak process RSS. GNU time is not required: the runner
uses `wait4` resource usage, including native worker threads. Quantiles are
per-run floor((N-1)*p/100); summaries take medians across three repetitions and
are not pooled percentiles. The ordinary two-second case has only two switch
samples per run: use the switch-storm case to study switching tails.

The offered client phases are evenly spaced, but Tokio's timer granularity can
batch wakeups. Producer lateness is exposed separately. There is no simulated
network jitter or actual timer-reactor source inside Hyphae. Saturation mode
(`--hz 0`) applies producer backpressure instead of rejecting a full queue, so
its latency figures have different semantics and should not be mixed with paced
results.

## Reproduce

```sh
# 10 scenarios × 3 implementations × 3 repetitions; 60 Hz, two seconds per run.
python3 tools/bench-scene-workload.py --output /tmp/scenes.jsonl
python3 tools/summarize-scene-workload.py /tmp/scenes.jsonl
python3 tools/analyze-scene-topology.py

# Focused switching correctness, including fresh branch construction.
HYPHAE_WORKER_THREADS=0 cargo bench -p hyphae --features scheduler \
  --bench scene_workload -- --backend hyphae --frames 8 --hz 0 \
  --switch-every 2 --dynamic-sinks on
cargo bench -p hyphae --features scheduler --bench scene_workload -- \
  --backend incremental --frames 8 --hz 0 --switch-every 2 --dynamic-sinks on
```

The runner preserves exact commands, source/fixture/lockfile hashes, compiler,
host CPU/affinity, raw quantiles and resource usage, including failed cases.
Runs are randomized with a fixed seed. The host is not reserved or CPU-pinned;
these short repeated runs are exploratory, not performance regression thresholds
or evidence of hard real-time guarantees.

## Interpretation

Use this bench to reject weak evaluator designs, assess switching costs, and
identify sensitivity to field expansion, input batching and encoding. A faster
result here justifies a small application-level adapter experiment; it does not
justify replacing Hyphae without testing Myko/report semantics, trigger ordering,
branch lifetime, native/WASM integration and real domain workloads.

The dependency map already rules out treating active scenes as independent
workers without examining cross-scene computation. The reference comparison
also gives a concrete baseline before adding arena storage or automatic
parallelism to Hyphae.

## Results from the first 60 Hz matrix

All **90 runs passed**, with **134,128 admitted input updates checked against the
reference evaluator**, out of 136,080 offered updates. The 1,952 rejected updates
were all in the deliberately overloaded switch-storm case. Ordinary 10/20/28-scene
and 12/14-client cases had no rejections or frame-budget misses. The deep-expansion
sensitivity case had two frame-budget misses across its nine runs. No incoherent
final result or oracle mismatch was observed; this does not establish transient
subscription semantics, because this harness reads settled outputs.

[Full per-case summary](scene-workload-summary.md) and
[raw JSONL](scene-workload-results.jsonl) preserve the individual repetitions.
The host was an AMD Ryzen 9 7900X3D, with Rust 1.97.1 and optimized benchmark
builds. These are medians across three runs of each run's p99:

| Full 28-scene fixture, 12 clients | Evaluation p99 | Scheduled → simulated fanout p99 | CPU seconds over ~2 s |
| --- | ---: | ---: | ---: |
| Hyphae, wave workers disabled | 5.713 ms | 8.037 ms | 0.871 |
| Hyphae, four wave workers | 3.809 ms | 5.859 ms | 1.031 |
| Incremental 0.2.8, sequential | 0.536 ms | 2.034 ms | 0.298 |

Wave workers helped this particular baseline's evaluation tails at increased CPU
cost. They did not consistently help across scenarios: the 14-client and 20-scene
cases had higher median p99 with four workers than with workers disabled. This
is evidence against assuming either universal improvement or universal harm from
parallel waves.

For the switch storm (14 clients, 60 whole-scene revision changes/s, plus 514
additional sink reconnection scopes), median switch-update p99 was 33.544 ms
without Hyphae wave workers, 29.980 ms with four, and 2.593 ms for Incremental.
Median per-run rejections were 372, 362 and zero respectively. This is an
adversarial reconstruction pattern, not a claim that production performs those
switches. Initial/final checks and all admitted updates still matched the oracle.

The result supports a focused Incremental/Myko semantic-adapter experiment before
investing in an arena or new automatically parallel scheduler. It does not prove
that Incremental can replace Hyphae: this fixture uses scalar arithmetic and
explicit stabilization, not real typed node logic, lossless trigger processing,
reactive query maps or cross-platform bindings.

## Validation and next work

- All 90 matrix runs and final topology validation passed.
- Focused benchmark Clippy with warnings denied, nightly Rustfmt, Python syntax
  compilation and JavaScript syntax checks passed.
- A final six-case runner smoke sweep and invalid-configuration checks passed.
  Additional saturation smoke runs exercised branch reconstruction and sink
  reconnection in both adapters. The final oracle is checked even when periodic
  verification is sampled.
- No runtime implementation or live production configuration was changed. New
  dependencies are benchmark/dev dependencies; the full workspace runtime suite
  was not run.

Before selecting an engine, test actual Myko report/query semantics, branch
lifetime and retained memory under long-running churn, stateful event ordering,
scene activation and multiple simultaneously executing sessions. A separate
collection-query fixture is needed to evaluate Differential Dataflow. The
read-only capture and backend-neutral topology should make those comparisons
more concrete without replaying private authored program values.

Tracked compatibility follow-up: `lv-1b18`. The remaining broader workload and
trace work continues under `lv-35d9`.
