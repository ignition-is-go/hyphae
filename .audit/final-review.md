# Thermo remediation closure review

## Verdict

The remediation closes every finding from the review of commit `4465c6dbe364bfee9f97136d64420549af14d800`. The final source preserves the published Rust API, passes both feature configurations, retains the specialized two-stage performance profile, and leaves no known code-quality follow-up from the audited scope.

## Finding closure

| Finding | Resolution | Proof |
| --- | --- | --- |
| `BoundedOutput` violated drop-oldest and accepted zero capacity | Reject zero capacity and evict before retrying a full dropping channel | Exact overflow-order and zero-capacity tests |
| Two join engines owned one lifecycle | Share `RegionHost`, transaction policy, publication, root registration, and guard ownership while retaining distinct typed kernels | Both arbitrary-N and specialized two-stage installers call `install_region_runtime` |
| `RegionRouter` represented impossible states | Replace coupled options with `RuntimeStorage::Serial` or `RuntimeStorage::Sharded`; make promotion an asserted transition | Join, promotion, ordering, and arbitrary-N tests |
| Compiler recovered from corrupt erased types | Assert sink/index downcasts and repeated binding before state mutation | Three invariant-panic tests plus compiler suite |
| `CellMap` duplicated projections and silently froze after poison | Share output installation and one ordered projection store; recover poisoned projection state explicitly | 26 CellMap tests |
| `MapDiff` had no traversal owner | Add `atomic_key`, `work_items`, `visit_leaves`, and `flatten_into`; migrate the recursive flatten helper and join callers | Nested-order test and join/runtime suites |
| `OnceCallback` used unnecessary unsafe synchronization | Use `Mutex<Option<F>>`, take under lock, and invoke after unlocking | Concurrent exact-once test; no production unsafe remains |
| Generated benchmark output dominated the tree | Remove generated captures, ignore future captures, and retain compact reports plus reproduction tools | 559 generated files removed; repository documentation retained |
| Large files concentrated unrelated ownership | Split compiler, CellMap, subscriber registry, join region, join runtime, and left join by responsibility | Every production Rust file is below 900 lines |

## Compatibility and behavior

- `cargo semver-checks` reports 223 passing checks, 31 skipped checks, and no required release update.
- `tools/verify_thermo_remediation.sh --full` passes the all-feature workspace tests, targeted behavior suites, formatting, and strict all-target Clippy.
- The no-default-feature workspace tests and strict all-target Clippy also pass.
- The all-feature Hyphae library suite passes 364 tests. The arbitrary-N public join suite passes three tests.
- Direct source scans find no production `UnsafeCell`, unsafe block, unsafe implementation, old optional router state, ignored relationship binding, or duplicate recursive flatten helper.

## Performance

The final two-stage Criterion interval is `[2.6027 µs, 2.6099 µs, 2.6171 µs]`; the baseline is `[2.6117 µs, 2.6201 µs, 2.6282 µs]`. The intervals overlap, the final point estimate is 0.4% faster, and Criterion detected no change.

For 100 single updates over 1,000 rows, baseline and final each perform 702 allocations totaling 28,006 bytes and 1,700 deallocations totaling 99,894 bytes. Output cardinality and checksum match.

## Independent review

An independent pstack thermo reviewer found no source-level correctness, compatibility, or safety blocker. It initially blocked closure because the checklist and decision trail had not yet recorded the final units; those artifacts are now current. Its remaining router observation was accepted: the duplicate serial branch was collapsed and promotion now asserts the state transition. The specialized kernel's remaining generic complexity is intentional because those types preserve compile-time plan bounds and avoid type erasure on the measured path.

The pstack comment-sicko pass reviewed every changed Rust file. Two findings were accepted: a decorative module banner was deleted, and a redundant broad `TwoStageKernel` suppression was removed. Public panic-contract documentation and narrow fail-stop/test suppressions were retained. No comment-only constraint remains from this change.

## Graph and coverage

The final source was indexed in full as `hyphae-current`. The index reported no skipped or partially parsed files, the checked evidence paths and `hyphae/src` scope had no recorded gaps, and the call graph contained zero cycles. These are best-effort graph signals; direct source scans and executable gates provide the completion proof.

## Attention

- The final code benchmark and allocation proof target `8ec08b9a6d78976596f9bc3767980f990162f722`; the later audit-only commit does not alter executable code.
- Raw allocation evidence is intentionally outside Git under `/home/trevor/.cache/hyphae-evidence/thermo-{baseline,final}`.
- No follow-up issue is required for the bounded audit scope.
