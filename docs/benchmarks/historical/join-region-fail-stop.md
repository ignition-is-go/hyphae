# Query-cohort join-region fail-stop publication atomicity

> Retention note: the raw captures referenced below are available from Git commit `4465c6dbe364bfee9f97136d64420549af14d800`. The current tree retains this decision record only.

Baseline SHA: `32178ee37ae556969a1592e5d6c784ed7d521967`
Candidate commit: assigned after the required pre-commit audit.

## Contract

Every installed left and right join-region callback executes its complete typed apply under one region mutex and one `catch_unwind(AssertUnwindSafe)`. A successful apply releases the guard before publishing returned changes. An apply unwind is resumed with its original payload only after Rayon has joined its scope, the router and its CompileContext query cohort are marked terminally poisoned, activity/test markers are cleared, and the guard is released. Later callbacks release their locks and create the fixed fresh panic `hyphae join region is poisoned after a prior callback panic` without invoking sinks, typed user closures, or indexes.

`CompileContext` owns a cloneable `QueryPoison`. Every physical-root dispatch checks it while holding the always-required QueryDispatch mutex and before active/queue decisions. This prevents another root or another RegionRouter in the same materialized query from continuing against a shared index advanced by a failed maintainer. Unrelated materializations own independent poison cohorts and remain healthy. Pre-poison events already queued behind the failing fanout are cleared by the existing QueryDispatch unwind guard.

This is **query-cohort fail-stop publication atomicity**, not recover-and-continue and not journaling/rollback of quarantined typed state. A failed region callback publishes none of that region's returned changes. It is still not query-wide sink rollback: if an enclosing fanout invoked an earlier independent downstream sink before a later region sink panicked, that earlier publication cannot be undone. Successful region commits release their guard before sink calls, so sink panics are deliberately not caught or converted into region poison.

## Panic/publication proofs

Targeted tests cover sequential apply, promotion replay, maintained right-root index write followed by projection panic, later JCons-stage panic, and forced balanced multi-shard Rayon panic. They assert original payload propagation; region/query poison; fixed later left/right/different-root fail-fast; stable closure/write counters; cleared parallel/test markers; and joined workers.

A public four-stage materialized JoinRegion test applies a real 10k source batch, synchronizes two final-stage shards with a barrier, observes sibling mutation and more than one named worker, catches the source callback panic, proves all workers exited, and asserts the real output map remains empty. A separate public reentrant test proves QueryDispatch discards the queued event and that a fresh different physical right root plus a fresh left root both hit cohort poison without invoking right-key/projection closures.

## Environment and method

- Host `malcolm`, Linux `7.1.3-arch2-2`, x86_64.
- Rust `rustc 1.97.1 (8bab26f4f 2026-07-14)`, LLVM 22.1.6.
- Criterion: separate fresh processes; frozen workloads; default 3 s warmup, 5 s measurement, 100 samples.
- Exact-SHA tiny-path baselines ran from a clean detached worktree at `32178ee`, removed after raw capture.

## Correctness gates

- Phase 5 golden preflight: PASS, retaining final-state digest `b394c0e6dc59b647f2dc1b05314b0043`, ordered-trace digest `959cdbd3c843b5ae31f7575208113219`, synchronous settling, and exact order.
- Focused join-region suite: **32 passed**.
- Full library/all features: **377 passed**.
- Strict workspace/all-target/all-feature Clippy with `-D warnings`: PASS.
- `cargo fmt --all -- --check` and `git diff --check`: PASS.

## Fresh measurements

| workload | exact 32178ee w1 | candidate w1 | candidate w4 | result |
|---|---:|---:|---:|---:|
| steady four-join batch 10k | n/a | 17.267–17.915 ms (17.594) | 7.0457–7.3934 ms (7.2155) | conservative `17.267 / 7.3934 = 2.335x`; midpoint `2.439x` |
| repeated typed right single | 4.3961–4.4187 us (4.4072) | 4.4032–4.4345 us (4.4182) | n/a | midpoint +0.25%; conservative `4.4345 / 4.3961 - 1 = 0.87%` |
| tiny left single | 3.6602–3.6801 us (3.6698) | 3.6694–3.6924 us (3.6799) | n/a | midpoint +0.28%; conservative `3.6924 / 3.6602 - 1 = 0.88%` |

The conservative scaling gate exceeds 1.5x and both incremental hot-path bounds are below 3%.

The raw correctness and benchmark outputs are available at the retention commit named above.
