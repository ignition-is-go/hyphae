# Thermo remediation plan

## Definition of done

The work is complete when every finding in `/tmp/hyphae-thermo/summary.md` has an implemented remedy, the published Rust API remains semver-compatible with commit `4465c6dbe364bfee9f97136d64420549af14d800`, targeted behavioral proofs pass, the complete workspace formatting/test/clippy gates pass, the two-stage join benchmark does not materially regress, the decision trail is independently reviewed, and the committed result is pushed with a clean worktree.

## Rigor

High. The change touches public reactive behavior, generic compile-time plans, concurrency, ordering, sharding, and repository history. Each unit must land independently and carry its own proof before the next begins.

## Checklist

- [x] Read the pstack principles and the applicable leaf skills.
- [x] Phase A: Frame the completion predicate, quantified scope, blockers, and rigor.
- [x] Phase B: Design the workflow, capture baselines, ground the join architecture, and run the architecture arena.
- [x] Phase C: Run each unit as a hypothesis, minimal change, real check, and keep-or-revert decision.
- [x] Unit 1: Add a reusable API and behavior verification harness with the pre-change baseline.
- [x] Unit 2: Correct `BoundedOutput` overflow and zero-capacity behavior without changing existing signatures.
- [x] Unit 3: Replace `OnceCallback` unsafe synchronization with a safe once-only owner.
- [x] Unit 4: Make query-compiler erased-type mismatches fail as internal invariants.
- [x] Unit 5: Add canonical, semantics-preserving `MapDiff` traversal operations and migrate duplicate helpers.
- [x] Unit 6: Consolidate `CellMap` projection installation and make poison behavior explicit.
- [x] Unit 7: Encode `RegionRouter` execution mode as an enum and simplify serial/sharded dispatch.
- [x] Unit 8: Put the specialized two-stage join behind the shared `JoinRegion` lifecycle while preserving public plan types and benchmark behavior.
- [x] Unit 9: Split remaining giant implementation files along ownership boundaries.
- [x] Unit 10: Remove generated benchmark bulk from the main tree while retaining compact reports and reproducible inputs.
- [x] Phase D: Keep `.audit/thermo-remediation.tsv` current with one row per decision and verified unit.
- [x] Phase E: Run the completion audit, independent trail review, full gates, semver comparison, benchmark comparison, and handoff.

## Throughput checkpoint

After Unit 4, compare actual elapsed time, diff size, and failing-gate rate with this plan. If later units cannot be finished in one coherent session, keep the same completion predicate, commit the verified units, file linked levi follow-ups for uncompleted units, and resume from the first unchecked unit.
