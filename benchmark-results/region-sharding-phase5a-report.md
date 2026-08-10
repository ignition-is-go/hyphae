# Phase 5a deterministic region-sharding evidence

Commit checkpoint: 224cc71 plus follow-up correctness fixes.

- Phase 5 preflight (`--all-features`): PASS; golden diff/order digest retained.
- Focused forced-router tests: PASS for initial/live promotion, N=3/N=8, Here/middle/distant roots, repeated-member right Batch, optional Some/None rekeys, shared-index Initial replacement and exactly-one write per event.
- Full library: 356 passed, 0 failed.
- Strict all-features clippy: PASS (`-D warnings`).
- Repeated typed tiny gate, workers=1: 4.4327 us midpoint vs 4.5042 us reference (-1.59%, within <3%).
- Four-join application steady batch, workers=1 repeat: 17.308 ms midpoint vs 18.815 ms reference (-8.01%); remains sequential because captured shard count is one.
- No Rayon shard iteration is included in this phase.

Raw outputs:
- region-sharding-phase5-preflight.txt
- region-sharding-full-lib.txt
- region-sharding-clippy.txt
- region-sharding-repeated-typed.txt
- region-sharding-four-join-w1-repeat.txt
