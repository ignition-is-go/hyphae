# Static MapQuery Phase Evidence Reconciliation

**Date:** 2026-08-11

**Release candidate runtime:** `5177ef5f290c8481ce45609ab45ac04e38c9970b`

**Documentation/conformance commit:** `a63c70ae41c298d16ebde60848e96f0906e480bb`

**Authoritative final evidence:** [`docs/benchmarks/historical/static-map-query-evidence.md`](../../benchmarks/historical/static-map-query-evidence.md)

## Why this record exists

The design requires exact before/after evidence and originally prescribed a
complete resource tuple for every implementation phase. The development
history did not satisfy that procedure literally: Phase 3 was split across
multiple relationship/root/index checkpoints without one standalone report,
and the early Phase-4 coordination report did not archive every allocation,
retention, and compiler-resource cell. This record does not relabel later data
as if it had been captured earlier and does not invent phase-local provenance.
Those checkpoints therefore make no independent release performance claim.

Release closure instead uses the exact frozen v3 and final candidate revisions
in one common, hashed harness. The consolidated evidence contains the full
latency, allocation, retention, correctness, codegen/profile, and serial
compile-resource tuple. This is an explicit process deviation from the ideal
phase-local discipline, not a deviation from the final correctness or
performance gates.

## Phase mapping

| Phase | Historical candidate/scope | Available checked report/evidence | Reconciliation |
|---|---|---|---|
| 0 | v3 runtime `7fcb1421`; harness later frozen at `8a0cadf` | `2026-08-10-static-map-query-phase-0-baseline.md` | Early report stopped dedicated batches at 1,000. Final `lv-671e` matrix supplies N/B through 10,000 and the reference/correctness counters; no unsupported Phase-0 completion claim depends on the smaller matrix. |
| 1 | `b496769` associated types and semantic projections | `2026-08-10-static-map-query-phase-1-results.md` | Direct before/after latency and allocation report exists. Final projection matrix independently confirms output/checksum/retention equivalence. |
| 2 | `3ad490b` monomorphized compiler | `2026-08-10-static-map-query-phase-2-results.md` | Direct Phase-1/Phase-2 latency/allocation report and codegen inspection exist. Final scoped LLVM audit supersedes early qualitative codegen evidence. |
| 3 | physical roots, typed relations, raw-root index reuse, optional FK through `c906298` | `2026-08-10-static-map-query-root-activation-results.md`, checked `rwlock-*` / `arbitrary-n-fk-*` artifacts, and source tests at `c906298` | **Process deviation:** no single full-tuple Phase-3 report. Exact correctness and checked targeted regression gates are retained; the exploratory optional-FK files that remain untracked are not cited as authoritative evidence. No independent Phase-3 resource improvement is claimed. Final repeated-relation allocation/retention/codegen results are authoritative. |
| 4 | two-join coordination then arbitrary-N typed regions through `20e9679` | `2026-08-10-static-map-query-phase-4-coordination-results.md`, `...phase-4-joined-projection-results.md`, checked `arbitrary-n-*` artifacts | Direct latency/allocation checkpoint evidence exists, but the coordination report marks missing historical retention/compiler cells. **Process deviation:** no claim is made for those missing cells. Final two-/four-join full tuples and N=3/N=8 correctness tests are authoritative. |
| 5 | deterministic whole-region sharding/fail-stop through `f6e2b57`, final runtime `5177ef5` | Phase-5 plans, `region-sharding-*`, `region-failstop-*`, `lv-7ff9-*`, `lv-2a0b`, `lv-33ed`, and `lv-515f` | Exact order/state, randomized forced serial/Rayon differential, terminal panic, two-join regression, first-enabled-row scaling, and large-batch scaling gates are independently archived. Final resource matrix supplies the full tuple. |

## Consolidated release comparison

The retained final report summarizes the allocation manifests, latency
provenance, compile measurements, environment, and checksums that freeze:

- historical v3 runtime `7fcb1421a36641e905cdcb0c30efa7867d03d6c0`;
- semantically equivalent v3 harness `8a0cadf2d68e267667c0cee22cf66921da9e1768`;
- final candidate runtime `5177ef5f290c8481ce45609ab45ac04e38c9970b`;
- exact commands, toolchain/platform, adapter and source hashes.

It contains 600 unique allocation rows per revision (four scenarios, five row
counts, five batch sizes, six phases), with 600/600 paired keys, zero duplicate
keys, zero output-cardinality/checksum mismatches, and zero teardown watermark
mismatches. It also contains paired 200-sample four-join latency, isolated
three-run serial compiler time/RSS/artifact results, scoped LLVM codegen, and a
zero-lost-sample allocation profile. The historical validator and checksums validate
the archive.

The final release claims are deliberately limited to what this evidence proves:

- final v3-to-candidate performance/resource equivalence or improvement gates;
- correctness of every measured semantic workload;
- no recurring allocation attributable to an intermediate stage collection;
- exact retained-watermark teardown;
- static hot-stage calls within the scoped probe/callback symbols.

It does **not** claim that every intermediate Phase-3/4 checkpoint had a fully
captured resource tuple or that the final matrix isolates each intermediate
commit's causal contribution.

## Go/no-go

**Go at the release level, with the Phase-3/4 phase-local reporting deviation
recorded above.** Future phases must capture their parent baseline before the
candidate and may not rely on this reconciliation as permission to omit it.
