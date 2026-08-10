# Static MapQuery deferred-root activation results

**Date:** 2026-08-10  
**Baseline:** `ec4796b`  
**Candidate:** working tree immediately after `ec4796b`

## Change

Query compilation now has separate construction and activation phases. Root
entry points are collected for the complete plan before any source subscribes.
Repeated uses of the same physical `(source, key type, value type)` compile to
one source subscription whose immutable typed sink list fans the diff into all
direct entry points. The root-boundary fanout performs no registry lookup,
locking, or allocation on updates. Shared-query activation retains ownership
of its own deferred upstream guards.

This is the prerequisite for interning relationship indexes during setup: the
compiler can now see every use before the first `Initial` diff executes.

## Equivalent benchmark

The `compiled_map_queries` harness gained
`compiled_query/repeated_relation_four_join/repeated_right_single`. It updates
one row in a dimension map used by four joins. The exact harness adapter was
applied to a detached baseline worktree at `ec4796b`.

Command on both revisions:

```text
cargo bench -p hyphae --bench compiled_map_queries -- repeated_right_single --sample-size 50
```

| Revision | 95% interval | Midpoint |
| --- | ---: | ---: |
| `ec4796b` before | 35.614–36.092 us | 35.853 us |
| candidate after | 32.308–32.608 us | 32.458 us |

The candidate is **9.47% faster** by interval midpoint (`1.105x`). This change
removes three physical source subscriptions but deliberately does not yet
claim relationship-index reuse: each downstream join still maintains its own
index until the typed relation-index compiler phase lands.

## Verification

- A compiler unit test proves two uses are interned into two physical roots
  total and activate into two guards total.
- An integration test proves a source reused by two joins has one subscriber,
  drives both joins synchronously, and unsubscribes when the output drops.
- `cargo test -p hyphae --lib --all-features`: 324 passed.
- `cargo clippy -p hyphae --all-targets --all-features -- -D warnings`: passed.
