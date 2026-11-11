# Owned incremental runtime experiment

The opt-in `local-runtime` feature exposes `hyphae::local`. It implements a
single-owner, demand-driven evaluator with mandatory equality cutoffs and
dynamic dependencies. The existing runtime and API remain available unchanged.
This is an experiment to establish semantics and a performance baseline before
adding automatic parallelism, rather than a complete replacement release.

## Transaction boundary

A transaction is an explicit finite batch of input writes. There is no timer,
debounce period, or implicit 60 Hz boundary. The application chooses when to
submit a batch. `stabilize()` is an empty transaction that also settles newly
observed nodes and consumes feedback from the preceding transaction.

Within a transaction, each evaluation step takes at most one unequal value per
input, settles all demanded outputs, and then invokes subscriptions. Repeated
unequal writes to one input produce ordered steps. Equal writes are skipped.
Different inputs share steps: `a=1, a=2, b=10, b=20` produces the coherent pairs
`(1,10)` and `(2,20)`, not a global sequence of four intermediate states. This
pairing is an explicit prototype policy; callers needing global event order
should represent that order as one input containing ordered occurrences.

An occurrence ID prevents equal payloads from collapsing, provided that ID is
retained wherever individuality matters. A downstream map that discards the ID
can still produce equal values and stop propagation. An undemanded branch does
not replay historical occurrences when reactivated: it sees the latest input.
`scan` consumes the current input on initial evaluation, then each demanded
unequal input value. Its state belongs to its branch scope.

Subscription callbacks receive the value and a transaction staging context.
Their writes wait for the next explicit transaction. This prevents recursive
evaluation and makes feedback progress an application decision. `has_feedback()`
reports pending work. Ordering among independent subscriptions is unspecified.

Staging failure applies none of that transaction's writes. Once evaluation
starts, failure poisons the runtime; it does not roll back previously delivered
effects or earlier completed evaluation steps. Panics poison and resume
unwinding. This is not a persistence transaction.

## Small example

```rust
use hyphae::local::{Error, Runtime};

fn example() -> Result<(), Error> {
    let mut rt = Runtime::new();
    let a = rt.input(1_u64)?;
    let b = rt.input(2_u64)?;
    let sum = rt.map2(&a.cell(), &b.cell(), |a, b| a.saturating_add(*b))?;
    let result = rt.observe(&sum)?;
    rt.transaction(|tx| {
        tx.set(&a, 10)?;
        tx.set(&b, 20)
    })?;
    assert_eq!(result.get(&rt)?, 30);
    Ok(())
}
```

`observe` demands a value without a callback. `subscribe` is the external effect
boundary. A cell has no untracked standalone `get`: reading an observer requires
the runtime. Construction supports inputs, constants, map, map2, n-ary fold,
stateful scan, and switch_map.

## Contracts and evidence

These are proposed contracts, not a claim that every API spelling is necessary.
Tests establish the chosen behavior and catch regressions; they cannot prove
that all alternative designs are inferior.

| Proposed contract | Reason and evidence | Status |
| --- | --- | --- |
| `Value: Clone + Eq + Send` | Every node compares its output before allowing downstream computation. Equality-cutoff tests count downstream executions; occurrence tests preserve distinct IDs. Floats require an explicit equality wrapper. | User-selected semantics; compile-fail example enforces `Eq`. |
| Mutation through `Input` and `Transaction` | Coherent diamonds and finite repeated-write batches need an explicit evaluation boundary. Tests cover coherent pairs, 20,000 unequal writes, and aborted staging. | Boundary required by this evaluator; batching/pairing policy remains revisable. |
| Owned runtime and declared dependencies | Exclusive ownership permits direct typed value reads without per-value locks. Foreign-runtime handles are rejected. Tests move a populated runtime across threads and use values that are `Send` but not `Sync`. | Performance experiment; does not prove shared ownership can never perform well. |
| Effects through subscriptions | All demanded outputs settle before publication; callback writes cannot reenter evaluation. Feedback and cycle tests exercise this boundary. | Core phase separation. No `tap` is exposed; this does not yet justify removing legacy `tap`. |
| Construction-only switch builder | A factory can declare a new branch but cannot mutate inputs, observe, or start transactions. A compile-fail example checks the restriction. | Restricts hidden dependencies and reentrancy; exact interface provisional. |
| Old switch scopes become invalid | Branch-owned nodes and dependent escaped handles are invalidated when replaced. Nested switches, escaped handles, stateful branches, and repeated rebuilds test lifecycle correctness. Shared unscoped inputs remain usable. | Deliberately strict policy inspired by Incremental scopes; not proven necessary. |
| Fail-stop evaluation errors | Partial computation/effect delivery cannot honestly promise rollback. Tests cover cycles and panics poisoning the runtime before further use. | Prototype failure policy; no signal-level error/completion protocol yet. |

No existing public API break has been made. Migration evidence is still needed
before replacing existing cell types or removing operators. In particular,
specialized collection queries, asynchronous operators, and lifecycle semantics
need representative adapters rather than mechanical renaming.

## Storage and scheduling

The runtime owns a generational slot arena and reuses retired slots and scopes.
Node metadata is in a vector; heterogeneous typed computation bodies remain
boxed per node. Typed values live in those bodies. This is not a fully contiguous
or compiled graph representation. Input staging still boxes each write.

Demand counts activate only observed dependencies. Dirty invalidation traverses
active descendants; an explicit work stack settles dependencies and uses change
epochs to skip computations below equal intermediate values. Deep evaluation,
retirement, and nested scope invalidation avoid recursive graph walks.
`switch_map` settles its selector, detaches its old dependency, retires its owned
scope, constructs or selects a new branch, then settles that branch. Active
stack cycle detection precedes publication. Selecting an existing unscoped
branch does not transfer ownership of that branch into the switch scope.

There are no runtime-owned threads, clocks, Tokio tasks, Rayon jobs, or global
graph state. `Runtime` is `Send` and is not `Sync`. Handles use reference-counted
leases; final drops enter a locked retirement queue. That bookkeeping lock is
not a lock around each value read or computation. Collection occurs at explicit
boundaries or through `collect()`.

This establishes a sequential baseline. Automatic parallelism still needs an
ownership/partition design for disconnected demanded regions, shared roots,
dynamic edge changes, and deterministic publication. `Send` alone is insufficient
to evaluate arbitrary shared dependencies concurrently, particularly with
`!Sync` values. Benchmarking partitions must include repartition cost and topology
churn, and should beat this sequential baseline before adding scheduler machinery.

## Validation and reproduction

The local test module contains 26 tests, including a generated 1,000-node dynamic
DAG checked against full recomputation, a 10,000-node chain, 1,500 nested switches,
bounded storage under 1,000 rebuilds, equality propagation, and feedback.

Validation also exposed an intermittent failure in the unchanged legacy
`test_concurrent_complete_signals`: four value callbacks instead of five. The
immediate isolated rerun passed, as did 100 isolated repetitions with the new
feature disabled. The cause is unresolved and tracked as `lv-b279`; this is
disclosed separately from the new runtime's passing semantic tests.
The complete all-features rerun passed all 421 unit tests, integration tests,
and doctests. Strict Clippy, targeted formatting, Python syntax checks, and the
WASM feature build also passed.

```sh
cargo test -p hyphae --all-features -- --test-threads=1
cargo clippy -p hyphae --all-features --lib --tests -- -D warnings
cargo check -p hyphae --target wasm32-unknown-unknown --features scheduler,async,profiling,local-runtime
```

The generic session workload benchmark remains available in this repository.
Application-specific fixtures and stack comparisons belong to their consuming
repositories; they are not part of this runtime API experiment.
