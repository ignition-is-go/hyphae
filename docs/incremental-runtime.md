# incremental-rs backend prototype

`incremental-runtime` exposes `hyphae::incremental`, an experimental local Cell
facade backed by incremental-rs 0.2.8. It does not replace the default API or the
existing `local-runtime` evaluator. Those remain available as compatibility and
performance baselines. This is a kernel prototype, not a completed library
migration or an application performance result.

The backend owns nodes, dependency tracking, equality cutoffs, observed demand,
height ordering, scoped dynamic `bind`, invalidation and recomputation. The facade
owns transactions, staged writes, effect delivery, and explicit thread bridges.
There is no second graph, wave propagator, thread pool or timer loop in this module.

## Contract and reasons for the API differences

| Surface | Contract | Reason and evidence |
| --- | --- | --- |
| `Runtime::input`, `Cell::map/map2`, `Runtime::fold` | Values need `Clone + Eq + 'static`; closures can contain local state. | The backend is local. Tests use `Rc` values and callbacks; no per-cell `Send`, `Sync` or `Debug` requirement is needed. A private wrapper satisfies the backend's diagnostic bound. |
| `Cell::observe` / `Observer::read` | Reading is explicit demand and returns the last stabilized value. It never evaluates. | The backend does not compute unobserved graphs; observers are unavailable until stabilized. Mandatory universal `get` would obscure this distinction. |
| `Runtime::transaction` | Explicit logical boundary, with no elapsed-time or frame-rate policy. | Distinct inputs can settle together without exposing inconsistent derived values. Diamond tests verify one settled evaluation. |
| Repeated input writes | Each input contributes at most one unequal value per step; equal values are skipped. Distinct inputs' nth unequal writes share a step. | Prevents A -> B -> A or two unequal occurrences from disappearing. A 1,000-transaction differential test compares full notification traces against the previous local evaluator. |
| `Runtime::subscribe` | First delivery follows stabilization. Effects run after stabilization, with feedback queued for the next explicit transaction. | Avoids recursive stabilization, which the backend prohibits, and separates state calculation from external effects. There is no implicit feedback loop to a fixed point. |
| `Cell::switch_map` | Uses scoped backend `bind`. The builder can construct branch constants; maps and folds declare dependencies. Old branch-scoped handles can become unavailable. | Tests replace branches 100 times and check captured resource retirement, inactive-branch demand, and invalidation of escaped observers. |
| `Runtime::publish` | Returns a local `Publication` lease; `publication.snapshot()` creates a thread-safe handle to the last published value. Requires `T: Send`, not `Sync`. | Synchronization is paid at an actual sharing boundary. Snapshot clones do not keep graph computation demanded after the lease is dropped. Snapshots are retained state, not lossless event queues. |
| `Runtime::input_bridge` | Returns a local bridge lease; `bridge.sender()` returns a bounded thread-safe input endpoint. `drain_inputs(limit)` schedules receipt explicitly. | No global runtime thread or per-cell channel is imposed. Round-robin draining is bounded; full/disconnected queues return the unsent value. Dropping the bridge or runtime disconnects the sender. |
| `with_name`, `Observer::graph_dot`, `Runtime::stats` | Backend labels, dependency export and work counters. | Observability does not require keeping a parallel graph representation. |

Successful transaction construction commits the staged writes. A construction
error commits none of them and retains existing pending feedback. Evaluation or
effect panic poisons the runtime; already executed steps cannot be rolled back.
A foreign dependency is rejected before construction where possible. A foreign
branch returned during `switch_map` is detected during evaluation and poisons the
runtime. Stored values must not change through shared interior mutability.
Computations must use declared dependencies and must not retain observers.

Only an individual published value is synchronized atomically. Publish an
aggregate if multiple fields need one coherent snapshot. A successful input send
means queued, not evaluated or acknowledged. Threads that require completion
must observe a published result or use an application-level acknowledgement.

## Example

```rust
use hyphae::incremental::Runtime;

# fn example() -> Result<(), hyphae::incremental::Error> {
let mut runtime = Runtime::new();
let input = runtime.input(0_u64);
let doubled = input.cell().map(|value| value.saturating_mul(2));
let publication = runtime.publish(&doubled)?;
let snapshot = publication.snapshot(); // Send + Sync
let bridge = runtime.input_bridge(&input, 256)?;
let sender = bridge.sender(); // Send + Sync

// Local writes and incoming cross-thread writes have explicit boundaries.
runtime.transaction(|tx| tx.set(&input, 5))?;
assert_eq!(snapshot.read(), Some(10));
// Other threads can sender.try_send(value); the owner chooses when to drain.
runtime.drain_inputs(128)?;
# Ok(()) }
```

## Validation and benchmark

```sh
cargo test -p hyphae --features incremental-runtime,local-runtime,scheduler \
  --lib --test incremental_compat -- --test-threads=1
cargo test -p hyphae --features incremental-runtime --doc incremental
cargo clippy -p hyphae --features incremental-runtime,local-runtime,scheduler \
  --lib --tests --example incremental_workload -- -D warnings
cargo run -p hyphae --release \
  --features incremental-runtime,local-runtime,scheduler \
  --example incremental_workload
```

The example compares shared eager, shared batched, custom owned and incremental-rs
execution on the same generic graph: 25 groups, each with 27 observed scalar
outputs, four changing six-stage chains, and 23 static outputs. Inputs advance
without sleeps; every 31st update also replaces dynamic branches. Each run warms
up for 100 updates, then times 2,000 updates, excluding construction. All modes
use the same synchronized output sink. Final values and each output's delivery
count are checked, including zero delivery from static outputs during measurement.

This compares cached computation nodes rather than a single fused arithmetic
closure. Shared eager mode has additional intermediate effect delivery when
configuration and data change separately. Shared batched and both local modes
coalesce those edits into one settled publication per output. The benchmark
prints delivery counts so this semantic difference remains visible.

There are no real timers, async I/O, database, query adapters or network clients
in this example. It is deliberately insufficient evidence for an end-to-end
application speedup. The earlier custom owned evaluator remains an important
baseline: replacing a custom scheduler with a maintained dependency is a
separate decision from winning every kernel benchmark.

## Initial measurements (2026-11-12)

Three consecutive native release runs of the command above, in milliseconds for
2,000 updates (construction and warmup excluded):

| Mode | Run 1 | Run 2 | Run 3 | Median | Deliveries per run |
| --- | ---: | ---: | ---: | ---: | ---: |
| Shared eager | 154.50 | 157.24 | 155.29 | 155.29 | 206,400 |
| Shared batched | 434.37 | 418.99 | 441.10 | 434.37 | 200,000 |
| Custom owned | 67.44 | 66.20 | 68.55 | 67.44 | 200,000 |
| incremental-rs facade | 75.53 | 72.10 | 75.92 | 75.53 | 200,000 |

All final-value and per-output delivery checks passed. The incremental-rs facade
is about 2.06 times faster than shared eager here, but about 12% slower than the
custom owned evaluator. Shared eager also delivers intermediate edit states;
its time is not an equal-notification-count comparison. The shared batched mode
has the same delivery count as the two local modes. These are small kernel
measurements, not an end-to-end performance claim or a statistically controlled
performance study. They support testing the dependency-backed kernel further;
they do not establish it as the fastest implementation.

Validation: 443 library tests and the 1,000-transaction differential test passed
with all three runtime features enabled. Three module doctests (including two
compile-fail ownership checks), strict Clippy for the library/tests/example, and
the default library check passed. The new Rust files pass nightly rustfmt.
Follow-up tasks: `lv-b46b` covers keyed queries/sampling; `lv-f2ae` covers depth
limits and long-lived resource behavior.

## Remaining migration work

The default Cell/pipeline API and reactive collection implementations still use
the existing backend. This prototype does not adapt keyed query diffs,
trigger-only sampling, event/error/completion streams, timer lifecycle or UI
bridges. It does not automatically parallelize a graph; independent runtimes
can be created on separate owner threads, connected by explicit bridges.

Before replacing the default backend, implement and measure consumer-neutral
collection and sampling adapters, validate reentrant effects and lifecycle
contracts for real consumers, and run complete server workloads. Investigate the
backend's height limit and resource behavior on deep graphs before declaring
API parity. Keep consumer-specific fixtures and performance claims downstream.
