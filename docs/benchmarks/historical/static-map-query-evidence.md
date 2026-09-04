# Final static `MapQuery` allocation, codegen, and compile evidence

> Retention note: the raw captures referenced below are available from Git commit `4465c6dbe364bfee9f97136d64420549af14d800`. The current tree retains this decision record and its compact validation summary.

**Issue:** `lv-671e`  
**Candidate runtime:** `5177ef5f290c8481ce45609ab45ac04e38c9970b`  
**Historical v3 harness revision:** `8a0cadf2d68e267667c0cee22cf66921da9e1768` (runtime ancestor `7fcb1421a36641e905cdcb0c30efa7867d03d6c0`)  
**Final evidence tooling:** `9803def418e0a5da965b8cac6f1369c71990605d`  
**Machine:** AMD Ryzen 9 7900X3D, rustc/cargo 1.97.1, Linux 7.1.3; exact capture in `environment.txt`.

## Verdict

The final candidate passes the specification's allocation/retention, static-stage dispatch, correctness, and two-/four-join latency gates. Compile wall time and executable size increased and are reported below; the isolated serial builds completed in all six runs, so these non-blocking metrics do not prevent release.

## Provenance and semantic comparability

The breaking API change requires revision-specific, frozen adapters. Both adapters construct the same four fixtures and execute the same setup, 100 single updates, batch, and teardown operations. Candidate typed foreign keys map `EvidenceParent` by its raw map key. The v3 four-join compatibility adapter therefore also uses the raw left map key at all four joins; using the row's `relation` field would be a different query. That correction is commit `cde71d0` and is present in the archived adapter hash.

An earlier exploratory matrix made that adapter mistake and is deliberately excluded. The authoritative pair here was recreated from empty output directories after the correction. Every allocation cell ran the exact manifest-hashed release binary in a fresh process.

Commands (the scripts reject partial/nonempty output and all build/output paths outside `/home/trevor`):

```console
python3 tools/run-map-query-evidence.py \
  --revision 8a0cadf --adapter v3 \
  --output /home/trevor/evidence/lv-671e-final/v3
python3 tools/run-map-query-evidence.py \
  --revision 5177ef5 --adapter candidate \
  --output /home/trevor/evidence/lv-671e-final/candidate

python3 tools/measure-compile-resources.py --revision 8a0cadf --adapter v3 ...
python3 tools/measure-compile-resources.py --revision 5177ef5 --adapter candidate ...
tools/inspect-map-query-codegen.sh 5177ef5 ...
```

The build command and matrix parameters are in the manifests; target paths, environment, adapter/runner hashes, artifact hashes, combined raw CSV outputs, and per-cell status/result hashes are in the archived JSON/CSV records.

## Matrix completeness and correctness

Each revision has exactly **600 unique rows**: 4 scenarios × 5 row counts × 5 batch sizes × 6 phases. `N` and `B` are independently `{1, 10, 100, 1,000, 10,000}`. The phases are `setup`, `build`, `materialize`, `single_updates`, `batch`, and `teardown`.

The retained [`data/static-map-query-validation.json`](data/static-map-query-validation.json)
reports:

- 600/600 keys paired; no duplicate or missing cells.
- **0 output-cardinality mismatches**.
- **0 output-checksum mismatches**.
- **0 teardown/entry-live-byte mismatches** in either revision.
- Scenario-entry and post-teardown live bytes are exactly projection 1,589, two-join 1,587, four-join 1,599, and rekey 1,591 bytes in both revisions.

The checksum is an allocation-free commutative aggregation of all matches, so it compares semantics without relying on unspecified historical match ordering. This matrix supplements the forced sequential/Rayon state-and-ordered-trace differential suite committed at `3d4db44`; it does not replace it.

## Allocation and retention results

Representative `N=1,000`, `B=100` raw counts:

| Scenario | Phase | v3 calls / bytes | Candidate calls / bytes | Change |
| --- | --- | ---: | ---: | ---: |
| Projection | Materialize | 7,551 / 1,407,368 | 1,505 / 210,444 | -80.1% / -85.0% |
| Projection | 100 updates | 3,000 / 168,800 | 900 / 34,400 | **-70.0% / -79.6%** |
| Projection | Batch | 875 / 75,659 | 338 / 28,219 | -61.4% / -62.7% |
| Two join | Materialize | 28,203 / 5,476,720 | 7,221 / 1,751,752 | -74.4% / -68.0% |
| Two join | 100 updates | 2,800 / 236,800 | 900 / 32,800 | -67.9% / -86.1% |
| Two join | Batch | 1,556 / 192,891 | 324 / 30,955 | -79.2% / -83.9% |
| Four join | Materialize | 26,589 / 7,242,348 | 28,114 / 3,907,768 | +5.7% / -46.0% |
| Four join | 100 updates | 5,200 / 466,400 | 4,200 / 356,000 | -19.2% / -23.7% |
| Four join | Batch | 2,957 / 328,315 | 2,751 / 256,175 | -7.0% / -22.0% |
| Rekey | Materialize | 30,206 / 5,486,400 | 24,401 / 4,942,520 | -19.2% / -9.9% |
| Rekey | 100 updates | 3,000 / 237,600 | 2,900 / 228,800 | -3.3% / -3.7% |
| Rekey | Batch | 1,660 / 194,023 | 1,464 / 178,439 | -11.8% / -8.0% |

The complete 600-row tables are available at the retention commit. Counts include required input/output `Arc` construction and final `CellMap` diff publication. Thus the 9 calls/update in projection are not nine compiler-stage collections. The scoped generated-code and profile audit below shows a single fused monomorph and no intermediate stage collection/dispatch. Across all 100 cells per revision, teardown returns exactly to the captured scenario-entry watermark: no plan, index, scratch-key, or output-row retention remains after teardown.

Selective projection materialization is proportional to matched output (50% in this fixture), rather than retaining every source row. Shared-relation exact-once maintenance is additionally proven by the forced differential/shared-index assertions at `3d4db44`; the allocation matrix confirms the final repeated-relation fixture's semantics and teardown.

## Generated code and sampling profile

Codegen used:

```text
RUSTFLAGS=-Csymbol-mangling-version=v0 -Ccodegen-units=1 -Cdebuginfo=1
```

The exact LLVM IR, assembly, and objdump are compressed but otherwise raw and hash-verified under `codegen-candidate/`.

- The 59-line `map_query_codegen_probe` IR contains 8 call/invoke instructions, all to concrete `@symbol` targets, including direct `CellMap<u64, Arc<Row>>::insert`.
- The complete nested `select -> map_values -> select -> map_values` root callback (`0x912e0..0x920a0`) contains 100 call instructions: 45 direct and 55 RIP-relative GOT imports for allocator/deallocator/panic/runtime functions. It has **zero register- or object-relative indirect calls** and therefore no indirect hot-stage dispatch.
- A `RawVec` reserve site belongs to the queued-event/reentrant slow branch, not to the uncontended fused stage path.
- `perf record -F 999 -g --call-graph dwarf` over 100,000 projection updates collected 282 samples with **zero lost**. It exposes one fully nested concrete callback, not separate dynamically dispatched stage frames. Dominant costs are `CellMap::maybe_prune_key_cells` (39.63%), `CellMap::insert` (27.10%), and final `CellMap::apply_diff_owned` publication (21.82%).

Together with the allocation reduction, no recurring allocation attributable to an intermediate stage collection was identified, and there is no indirect dispatch between the four hot stages. Required final publication/`Arc` allocations remain. Root subscription callbacks and allocator/runtime imports are legitimate graph/runtime boundaries, not erased compiler stages.

## Latency gates

The audited final two-join 100-sample CI at `5177ef5` is **2.7438–2.7666 µs**, versus v3 **4.1174–4.1352 µs**: a conservative **32.81%** improvement ([`two-left-join-latency.md`](two-left-join-latency.md)).

The final paired four-join measurements use exact archived checkouts, 200 samples, and a 10-second measurement window. Candidate `5177ef5` is **3.6375–3.6588 µs** (point 3.6481); the freshly rebuilt archived-v3 harness is **5.5550–5.5795 µs** (point 5.5670). Candidate point versus v3 point improves **34.47%**; candidate upper versus v3 lower conservatively improves **34.14%**. The historical latency provenance records both exact commands, commits, benchmark source hashes, binary hashes/sizes, and paths. The v3 benchmark source hash `e9dc04a...` is also the immutable Phase-0 harness hash recorded in `docs/superpowers/plans/2026-08-10-static-map-query-phase-0-baseline.md`.

Three preceding default 100-sample candidate repetitions are retained rather than hidden: their point estimates were 3.7212, 4.2245, and 3.7331 µs. The second repetition was materially slower (4.0827–4.3904 µs); its cause was not established. A further 200-sample run from the tools-only working-tree descendant measured 3.6986–3.7208 µs and is also retained. The fresh v3 result is slower than the original Phase-0 CI (5.3422–5.3721 µs); even candidate upper against that older lower bound improves **31.51%**.

The Phase-5 sequential and parallel threshold/scaling gates and exact ordered equivalence are separately archived and audited in the `lv-33ed`, `lv-7ff9`, and `lv-2a0b` evidence; this report does not relabel allocation timing as Criterion latency.

## Compile resources

Each revision was archived into a clean checkout and compiled serially (`CARGO_BUILD_JOBS=1`, empty isolated target, locked/offline release example). Runs alternated v3/candidate.

| Revision | Wall seconds (3; median) | User seconds (3; median) | System seconds (3; median) | Peak RSS KiB (range) | Exact executable bytes |
| --- | --- | --- | --- | ---: | ---: |
| v3 | 53.061, 52.245, 51.859; **52.245** | 48.657, 47.859, 47.321; **47.859** | 4.490, 4.512, 4.615; **4.512** | 396,124–428,764 | 1,227,728 |
| Candidate | 63.589, 57.539, 57.424; **57.539** | 55.721, 53.137, 53.036; **53.137** | 4.984, 4.531, 4.468; **4.531** | 409,960–429,392 | 2,476,928 |

Median clean wall time increased **10.13%** and the example executable is **2.017×** as large. RSS ranges overlap. Artifact hashes are stable within each revision. The specification explicitly records these as non-blocking unless compilation fails; all six isolated builds succeeded.

## Retained evidence

The compact validation counters are retained as [`data/static-map-query-validation.json`](data/static-map-query-validation.json). The complete matrices, compile measurements, code-generation captures, profiles, and checksums remain available at the retention commit named above.
