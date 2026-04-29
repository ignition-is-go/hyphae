# rship Blast Radius for Pipeline Migration

Audit date: 2026-04-24
Audited tree: `/home/trevor/Code/rship/` (read-only; no cargo, no edits)

## Grep counts

Single-line patterns from the task brief — all zero or near-zero because rship
chains are heavily multi-line and idiomatic chains tend to either return from a
helper (`fn -> Cell<...>`) or feed into a stateful operator, not bottom out on
`.subscribe(...)` directly:

| Pattern (single-line) | Count |
|---|---|
| `\.map(.*\.subscribe` | 0 |
| `\.filter(.*\.subscribe` | 0 |
| `\.try_map(.*\.subscribe` | 0 |
| `\.tap(.*\.subscribe` | 0 |
| `\.map_ok|map_err|catch_error|unwrap_or(.*\.subscribe` | 0 |
| `\.map([^)]*)\.with_name` | 0 |
| `\.filter([^)]*)\.with_name` | 0 |
| `\.map([^)]*)\.scan|debounce|window|pairwise` | 0 |
| `fn .* -> Cell<` (rough upper bound on Cell-returning fns) | 223 |

Multi-line / contextual counts (ripgrep `-U --multiline-dotall`, paren-bounded
where possible — actual figures are lower bounds because rust's brace-balanced
chains defeat regex):

| Pattern | Count |
|---|---|
| Files importing `MapExt` | 92 |
| Total `MapExt` references | 110 |
| `FilterExt` references | 14 |
| `TapExt` references | 1 (`libs/link/core/src/folder.rs`) |
| `TryMapExt` / `MapOk` / `MapErr` / `CatchError` / `UnwrapOr` references | 0 |
| `.subscribe(` callsites | 62 |
| `.with_name(` callsites | 12 (5 of which already follow a hyphae `.map(...)`) |
| `.materialize(` callsites | 0 (API does not yet exist in rship's pinned hyphae) |
| `fn ... -> Cell<...>` signatures total | 339 (rg multi-line aware) |
| `fn ... -> Cell<...>` whose body contains `.map(` somewhere | 39 (28 distinct files) |
| `fn ... -> Cell<...>` literally ending in `.map(...)\n}` | 31 |
| `fn ... -> Cell<...>` literally ending in `.filter(...)\n}` | 3 |
| `.switch_map` / `.merge_map` callsites (closures must return Cell) | 95 |
| Top file by hyphae pure-op density | `binding_node.rs` — 72 occurrences of `.map(/.filter(/.tap(/.try_map(` |
| Sum of `.map(/.filter(/.try_map(/.tap(` across hyphae-using files | 691 (very loose upper bound — most are `Iterator::map`/`Option::map`) |

## Total candidate fix sites: ~40–60

A defensible estimate after reading samples:

- **~31 `fn -> Cell<...>` whose tail is `.map(...)`** — each needs one
  `.materialize()` appended.
- **~3 `fn -> Cell<...>` whose tail is `.filter(...)`** — same fix.
- **5 explicit `.map(...).with_name(...)` chains** in
  `libs/entities/nodes/src/binding_node.rs` (lines 564, 625, 629, 1155, 1250).
  Each needs `.materialize()` before `.with_name`.
- **1 `.tap(...).subscribe(...)` chain** in
  `libs/link/core/src/folder.rs:82-84`. Insert `.materialize()` before
  `.subscribe`.
- **1 `.map(...).distinct()` chain** at
  `libs/entities/engine/src/node_executor/playback_endings.rs:43-56`.
  `.distinct()` is a stateful op and is `Watchable`-bounded; insert
  `.materialize()` between `.map` and `.distinct`.
- **1 `.map(...).distinct_until_changed_by(...)` chain inside a `switch_map`
  closure** at `libs/sync/src/reports.rs:50-84`. Closure must return Cell;
  insert `.materialize()` between `.map` and `.distinct_until_changed_by`.
- **Unknown additional sites inside `.switch_map` / `.merge_map` closures.**
  rship has 95 of these, and several closures end in `.map(...)`
  (`runtime/src/nodes/target_action.rs`, `external/src/nodes/multi_action.rs`,
  `engine/src/node_executor/multi_action.rs`, etc.). Each such trailing `.map`
  in a closure that must return `Cell<U, CellImmutable>` needs
  `.materialize()`. Spot-checked count: 5–15 more. Treat as part of the
  mechanical pile, not as a separate concern.

Final ballpark: **40–60 mechanical `.materialize()` insertions across ~30
files.** All other hyphae call sites (`Cell::new(...).with_name`, stateful op
chains, raw `cell.subscribe`, `switch_map` closures already returning a Cell)
are unaffected.

## Sampled call sites

1. `libs/entities/lists/src/nodes/list_repeat.rs:32-38` — `make_cell` returns
   `Cell<BindingDatagram, CellImmutable>`; body is
   `cell.join(...).map(...)`. Fix: append `.materialize()`. Trivial.
2. `libs/entities/cameras/src/nodes/camera_info.rs:30-39` — `make_cell` body
   is a single `cell.map(closure)`. Fix: `.materialize()`. Trivial.
3. `libs/entities/physical/src/nodes/region_detect.rs:118-143` — `make_cell`
   body is `point_cell.map(closure)`. Fix: `.materialize()`. Trivial. (Note:
   `make_exec_cell` further down already returns `Cell::new(...).lock()` and a
   stateful chain, no fix needed there.)
4. `libs/entities/physical/src/nodes/velocity.rs:50-79` — `make_cell` body is
   `value_cell.map(closure)`. Fix: `.materialize()`. Trivial.
5. `libs/entities/logic/src/nodes/gate.rs:27-36` — `make_cell` ends with
   `value_cell.join(&open_cell).filter(...).map(...)`. `.filter` returns
   Pipeline, then `.map` returns Pipeline. Fix: `.materialize()`. Trivial.
6. `libs/entities/color/src/nodes/build_color.rs:17-32, 36-61, 64-75` — three
   separate Cell-returning helpers each ending in a `.map(...)` chain. Each
   needs `.materialize()`.
7. `libs/entities/nodes/src/binding_node.rs:548-565` — `compute` fn ends with
   `ctx.report(...).map(...).with_name(label)`. Fix:
   `ctx.report(...).map(...).materialize().with_name(label)`. Trivial.
8. `libs/entities/nodes/src/binding_node.rs:623-629` — chain
   `base_cell.map(...).with_name(...).map(...).with_name(...)` will require a
   `.materialize()` after each `.map(...)`. Two insertions in this segment.
9. `libs/entities/engine/src/node_executor/playback_endings.rs:43-56` — pure
   `.map(...)` followed by stateful `.distinct()`. Fix: `.materialize()`
   between them. Trivial.
10. `libs/sync/src/reports.rs:50-84` — `switch_map` closure body returns
    `sampled.map(...).distinct_until_changed_by(...)`. The closure type
    requires Cell, and `distinct_until_changed_by` is Watchable-bounded.
    Fix: `.materialize()` between `.map` and `.distinct_until_changed_by`.
    Trivial.
11. `libs/link/core/src/folder.rs:82-84` —
    `hyphae::interval(...).tap(...).subscribe(...)`. Fix: `.materialize()`
    before `.subscribe`. Trivial. Bonus value note: the `_sync_task` field
    holds the resulting `SubscriptionGuard`, so behaviour must remain a
    materialized cell with a single subscriber — `.materialize().subscribe`
    preserves that exactly.
12. `libs/entities/assets/src/file.rs:719-746` — `compute` returns
    `ctx.query_map(GetAllFiles {}).items().map(closure)`. `.items()` returns
    Cell (per `cell_map.rs:745`), so `.map` produces a Pipeline that must be
    `.materialize()`'d before the function returns. Trivial.

## Non-trivial sites

None located during sampling. Every break observed is a one-line
`.materialize()` insertion. Specifically:

- No call site clones the post-`.map` value as a Cell (Pipeline isn't `Clone`-
  semantically equivalent to Cell, but every observed downstream is either a
  return-from-fn, a `.subscribe`, a `.with_name`, or a stateful op — all
  satisfied by inserting `.materialize()`).
- No call site relies on the post-`.map` value's identity (no `Arc::ptr_eq`,
  no manual subscription bookkeeping that assumes intermediate cells).
- The `switch_map` closure case (`reports.rs`) is fully resolved by the
  `.materialize()` rule and does not change semantics: the inner Cell created
  by `.materialize()` is immediately re-subscribed by `switch_map`, which is
  what the previous behaviour already produced via the chained `.map`'s
  intermediate cell.
- The chained `.map(...).with_name(...).map(...).with_name(...)` sequences in
  `binding_node.rs` will require `.materialize()` insertions at each junction;
  this preserves prior behaviour (each `.with_name` previously named the
  intermediate Cell that `.map` allocated; now it names the explicitly
  materialized Cell). Trace-label observability is preserved.

## Recommended migration approach

Mechanical `.materialize()` insertion at the ~40–60 detected sites. No
deeper refactor needed; no observed call site requires changing types beyond
adding the trailing `.materialize()`. Recommended sequencing once rship
bumps the hyphae dep:

1. Let the compiler drive the audit: rship's `cargo check` will surface every
   genuine break with a "expected `Cell<...>`, found `Pipeline<...>`" diagnostic.
2. Apply `.materialize()` per error. Most fixes are one line each.
3. After green compile, scan diff for places where two adjacent `.materialize()`
   calls bracket a single `.with_name(...)`/stateful op — if observability or
   debugging benefits from a named intermediate, keep both; otherwise the inner
   one can be removed for clarity.

Total expected migration effort: an afternoon of mechanical edits plus a
build/test cycle.
