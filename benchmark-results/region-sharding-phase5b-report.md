# Phase 5b adaptive left-root region execution

Candidate parent SHA: `0b012fb18c8683b9df74118ae0c70d9b75ced68d`  
Review-candidate code diff SHA-256: `60d396746ef96057d203aa7faab9b8ac98e694fd129cf77af0587f39dc2f074c`  
Commit SHA-to-be: assigned only after the mandatory pre-commit review (committing earlier was explicitly prohibited).

## Environment and method

- Host: `malcolm`, Linux `7.1.3-arch2-2`, x86_64.
- Rust: `rustc 1.97.1 (8bab26f4f 2026-07-14)`, LLVM 22.1.6.
- Criterion: separate fresh `cargo bench` processes for every worker value; 2 s warmup, 5 s requested measurement, 50 samples, Criterion default confidence interval. Criterion extended some collections to satisfy sampling.
- Workload was not changed: `compiled_query/four_join_application/steady_batch/10000`; rekey corroboration used frozen `rekey_10pct/10000`.

## Correctness and golden preflight

`cargo test -p hyphae --test four_join_application_preflight --all-features`: PASS. The checked-in workload assertions retained:

- final-state digest `b394c0e6dc59b647f2dc1b05314b0043`
- ordered-trace digest `959cdbd3c843b5ae31f7575208113219`
- synchronous settled return and exact diff order.

Full library: 370 passed. Strict workspace all-target/all-feature Clippy passed. `cargo fmt --all -- --check` passed.

## Frozen benchmark intervals

| workers | steady 10k interval | midpoint | scaling vs w1 midpoint |
|---:|---:|---:|---:|
| 0 | 15.399–16.860 ms | 16.126 ms | 0.766x |
| 1 | 12.142–12.575 ms | 12.348 ms | 1.000x |
| 2 | 10.160–10.608 ms | 10.372 ms | 1.191x |
| 4 | 6.3006–6.9471 ms | 6.6249 ms | 1.864x |
| 8 | 7.7327–8.3297 ms | 8.0442 ms | 1.535x |

The conservative w4 gate is `12.142 / 6.9471 = 1.748x` (>=1.5x). Scaling is positive from one through the useful/default cap of four workers; zero workers is a distinct no-pool fallback and eight workers honestly shows saturation/regression versus four.

Rekey corroboration: w1 `17.117–18.183 ms` (17.620 midpoint), w4 `7.7709–8.5987 ms` (8.1828 midpoint), 2.153x midpoint.

Tiny repeated typed, workers=1: `4.3791–4.4146 us`, midpoint `4.3957 us`, 2.41% below the 4.5042 us reference (within 3%). Tiny left-root `repeated_relation_four_join/single`, workers=1: `3.5612–3.5724 us`, midpoint `3.5668 us`; the required production key-sequence maintenance caused no regression in this run.

Complete verbatim stdout/stderr is checked in as `region-sharding-phase5b-steady-w{{0,1,2,4,8}}.txt`, `...-rekey-w{{1,4}}.txt`, `...-tiny-w1.txt`, `...-tiny-left-w1.txt`, `...-preflight.txt`, `...-full-lib.txt`, `...-clippy.txt`, and `...-fmt.txt`. Commands appear in this report and are reconstructible from each descriptive filename; all benchmark processes used the exact invocation above with the filename's worker value.

## Explicit Phase 5c follow-ups

Phase 5 is **not complete**. Worker-panic transactional rollback/recovery is unresolved: a worker panic can leave sibling persistent shard runtimes partially advanced even though publication is caller-only. Tracked as `lv-b5ec` (P0); Phase 5b makes no panic-safety claim.

The 160k/96k enter/exit constants are conservative static policy values informed by the frozen 10k measurements and validated by policy tests, but a benchmark at the actual threshold boundary was not run. Near-threshold calibration is explicitly deferred to `lv-7ff9`; the report does not claim crossover calibration from the 10k result alone.
