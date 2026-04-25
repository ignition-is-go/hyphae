# rship Blast Radius for CellMap Query Plans Migration

Audit date: 2026-04-25

Read-only audit of `/home/trevor/Code/rship` against the new `MapQuery` API
(tasks 1–12 of the CellMap Query Plans refactor in hyphae). No rship code was
modified and no cargo invocation was performed against rship.

## Grep counts

Run from `/home/trevor/Code/rship`:

```bash
grep -rn "\.inner_join_by\|\.inner_join(\|\.inner_join_fk" libs/ apps/ --include="*.rs"
grep -rn "\.left_join_by\|\.left_join(\|\.left_join_fk"   libs/ apps/ --include="*.rs"
grep -rn "\.left_semi_join_by\|\.left_semi_join("         libs/ apps/ --include="*.rs"
grep -rn "\.multi_left_join_by"                            libs/ apps/ --include="*.rs"
grep -rn "\.project(\|\.project_many(\|\.project_cell("    libs/ apps/ --include="*.rs"
grep -rn "\.select(\|\.select_cell("                       libs/ apps/ --include="*.rs"
grep -rn "\.count_by(\|\.group_by("                        libs/ apps/ --include="*.rs"
```

| pattern                       | raw count | real count | notes |
|-------------------------------|-----------|------------|-------|
| `.inner_join*`                | 1         | 1          | only `inner_join_by` |
| `.left_join*`                 | 27        | 27         | all `left_join_by` |
| `.left_semi_join*`            | 1         | 1          | only `left_semi_join_by` |
| `.multi_left_join_by`         | 1         | 1          | |
| `.project*`                   | 52        | 51         | -1 false positive in `apps/link-ui/target/.../opencv/core.rs` (build artifact, doc-comment example `pca.project(vec, coeffs)`) |
| `.select*`                    | 18        | 17         | -1 false positive `apps/link-cli/src/main.rs:586` (`list_state.select(Some(...))` from ratatui) |
| `.count_by` / `.group_by`     | 8         | 8          | all `group_by`; no `count_by` uses found |
| **Total candidate sites**     | **108**   | **106**    | |

All real call sites live under `libs/entities/*` (one apps file is purely a
ratatui false-positive). The hot files:

- `libs/entities/assets/src/file.rs` — heaviest user (~30 sites).
- `libs/entities/assignment/src/target_tree.rs` — ~20 sites, deepest chains.
- `libs/entities/nodes/src/binding_node.rs` — ~6 sites.
- `libs/entities/assignment/src/instance_assign.rs` — 7 sites.
- `libs/entities/value-track/src/views.rs`, `external/src/target.rs`,
  `ui/src/bootstrap_view.rs`, `machine/src/machine.rs` — handful each.

## Sampled call sites

Migration shorthand:
- **terminal-materialize**: chain ends at `build_cell` return (`TypedViewCellMap`),
  add `.materialize()` at the tail.
- **rhs-clone**: RHS is reused later — pass `b.clone()` instead of `&b` and
  drop the borrow.
- **mid-materialize**: an intermediate is consumed by something that requires
  a `CellMap` (e.g. `.size()`, `.entries()`, `.get(...)` inside a closure),
  or is `.clone()`-d for use in another query — needs `.materialize()` at
  that boundary.

1. `libs/entities/value-track/src/views.rs:51-78` — `instances.left_join_by(&lanes, ...).left_join_by(&keyframes, ...).project(...)` returned from `build_cell`. Fix: drop `&` on `lanes` / `keyframes`, append `.materialize()` to the final `.project(...)`. Both RHS sources used once. **terminal-materialize.**

2. `libs/entities/assignment/src/target_tree.rs:79-94` — `all_instances.left_join_by(&cluster_assigns, ...).select(...).project(...)` then bound to `let all_instances = ...` and reused at line 97 inside `project_cell`. Result is consumed as a `CellMap` (passed back to `build_cell`'s return type after the `project_cell`). Fix: `.materialize()` at end of the chain. `cluster_assigns` is single-use, no clone needed. **mid-materialize** (let-binding consumed by another query operator).

3. `libs/entities/assignment/src/target_tree.rs:201-246` — long chain on `all_targets` joining `actions_by_target`, `emitters_by_target`, `online_statuses` with interleaved `.project(...)`s, bound to `targets_with_joins`. Then fed into `multi_left_join_by` at line 305 with `&instances_with_assignment`. Fix: drop borrows on the four RHS maps; the chain stays plan-shaped (no `.materialize()` mid-flight) until the terminal `.project(...)` at line 318 which gets `.materialize()`. **terminal-materialize, mostly mechanical.**

4. `libs/entities/assignment/src/instance_assign.rs:305-315` — `ctx.query_map(...).select(|ass| ass.assigned).size()`. `.size()` is a `CellMap` method, not on `*Plan`. Fix: insert `.materialize()` between `.select(...)` and `.size()`. **mid-materialize at boundary with `.size()`.**

5. `libs/entities/assignment/src/instance_assign.rs:358-381` — `instances.left_join_by(&assign_active, ...).select(...).project(...).left_join_by(&connected_clients, ...).select(...).select(...).project(...)` returned from `build_cell`. Fix: drop borrows on both RHS maps, terminal `.materialize()`. Each RHS used once. **terminal-materialize.**

6. `libs/entities/assets/src/file.rs:178-228` — `asset_paths.left_join_by(&files_by_asset, ...).project(...)` then `.left_join_by(&transfers_by_asset, ...).project(...)` returned. Three named intermediates; each RHS used once. Fix: drop borrows, terminal `.materialize()`. **terminal-materialize.**

7. `libs/entities/assets/src/file.rs:893-917` — `instances_map.project(...).group_by(...).project(...)` bound to `active_service_ids`, then `active_service_ids.left_join_by(&targets_map, ...).project_many(...)` bound to `active_target_ids`. **`active_target_ids` is later `.clone()`-d into a `select_cell` closure at line 936 and queried with `.get(...)`** — that requires a `CellMap`. Fix: terminate `active_target_ids` with `.materialize()`. **mid-materialize because of `.get(...)` usage.**

8. `libs/entities/assets/src/file.rs:919-943` — `target_nodes` chain ends with `.project(...)`, but `target_nodes` is later cloned and `.get(...)`-ed at line 974. Same pattern as (7). Fix: `.materialize()` at end of `target_nodes`. **mid-materialize.**

9. `libs/entities/assets/src/file.rs:979-1040` — `query_pins`, `sessions_by_constant`, `multiplex_sources` long chain ending in `multiplex_sources.project_cell(...)` returned from `build_cell`. The middle uses fluent let-bindings, no `.size()`/`.get()` calls in between. Fix: terminal `.materialize()`. **terminal-materialize.**

10. `libs/entities/nodes/src/binding_node.rs:2330-2400` — `nodes.left_join_by(&positions_by_node, ...)` → `nodes_with_positions`; `binding_node_tag_assigns.left_join_by(&tags_by_id, ...)` → `tag_assigns_with_tags`; `.project(...).group_by(...).project(...)` → `tags_by_node`; `nodes_with_positions.left_join_by(&tags_by_node, ...)` → `nodes_with_positions_and_tags`; terminal `.project_cell(...)`. Each let-bound intermediate is used exactly once as the LHS of the next operator — the consume-`self` API works without any forced materialization mid-chain. Fix: drop borrows on all three RHS maps, terminal `.materialize()`. **terminal-materialize.**

## Non-trivial sites

The audit surfaced two flavours of non-trivial migration. Neither is
structurally hard, but both require eyeballing rather than blind sed:

- **`.size()` / `.entries()` / `.get(...)` boundaries** — any chain whose
  terminal feeds a `CellMap`-only method (i.e. anything not on the `MapQuery`
  trait) must `.materialize()` before that call. Confirmed sites:
  - `libs/entities/assignment/src/instance_assign.rs:313` (`.select(...).size()`)
  - `libs/entities/assets/src/file.rs:917` (`active_target_ids` → `.get(...)` in closure)
  - `libs/entities/assets/src/file.rs:943` (`target_nodes` → `.get(...)` in closure)
  - Likely a handful more — repo has 112 total `.size()`/`.entries()` calls
    and 530 `.get(...)` calls; only the ones whose receiver is the result of
    one of our operators are affected. A focused grep at migration time will
    catch them.

- **Let-bindings consumed multiple times** — when an intermediate name is
  used more than once (e.g. cloned into closures), the consume-`self` API
  forces a `.materialize()` decision: replicate the plan or run it once and
  share the resulting `CellMap`. The latter is almost always the right call.
  Same three sites as above plus `target_tree.rs:79-94` `all_instances`
  rebinding pattern.

No site discovered requires architectural changes. There is no recursion,
no return type mismatch beyond `TypedViewCellMap` (which always wants a
materialized `CellMap`), and no place where a function returns a chain of
operators *as* a plan — every chain terminates within `build_cell`.

## Recommended migration approach

Mostly mechanical:

1. Per file, walk the chains top-down and apply: drop `&` on RHS arguments,
   add `.clone()` on RHS where it's used by a later operator, append
   `.materialize()` at terminals (every `build_cell` return) and at the
   `.size()` / `.entries()` / `.get(...)` boundaries listed above.
2. Targeted review of the ~3 non-trivial mid-materialize sites in
   `assets/src/file.rs` and `instance_assign.rs:313`.
3. A repo-wide build with the new hyphae version will surface anything
   missed (the consume-`self` signature change makes the compiler vocal —
   borrow-vs-move errors will pin every remaining site).

Estimated effort: ~106 mechanical edits + ~30 minutes of focused review on
the half-dozen non-mechanical sites. Single-PR migration is realistic.
