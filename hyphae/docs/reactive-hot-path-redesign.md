# Reactive hot-path redesign

**Status:** Phase 1 implemented; Phases 2–3 designed, staged.
**Motivation:** live CPU profile of `rship-server` on the HRLV prod project
(linux `perf`, 999 Hz, frame pointers on) shows **83.3 % of on-CPU samples
(17,636 / 21,165)** in the hyphae reactive notify + `Cell`/`Arc` drop cascade.
Not I/O — Postgres/publish paths were 0 samples. Still dominant on v1.1.1
(after the CellMap `key_cells` fix and the 1.1.0 `ArcSwap`→`Mutex` change).

The hot stacks, both on binding-node value cells
`Cell<Vec<(Arc<str>, BindingValue)>>`:

1. **Subscriber notify + registry churn** — `notify → next → eq<(Uuid, Arc<Subscriber<…>>)>`
   (~170k / 159k frame-samples). The `eq<Uuid>` is the copy-on-write
   `.filter(|(i, _)| *i != id)` scan in `subscribe`/`unsubscribe`: every
   subscribe clones the whole `Arc<Vec>` and every unsubscribe linearly scans
   and rebuilds it. When `switch_map`-style operators resubscribe per fire,
   that O(n) churn runs *inside* the notify cascade.
2. **Cell/Arc drop cascade** — `drop<CellInner<Vec<…>>> → fetch_sub →
   drop_in_place<Arc<CellInner<…>>>` (~159k). Inner cell towers are torn down
   and rebuilt on every fire — atomic-refcount churn.
3. **~436-frame-deep stacks** — map/join/switch_map towers allocate an
   intermediate `Cell` at every hop, so each notify pays for the whole tower.

Root cause: a massive reactive graph where every change triggers linear-scan
notification + cell teardown. Ties together the co-symptoms — 9.5 GB reactive
graph RSS, ~9,200/sec `ReportResponse` fan-out under animation, 83 % CPU.

---

## Phase 1 — Index the subscriber registry (IMPLEMENTED)

**Target:** kill the O(n) `eq<Uuid>` scan (cost #1).

### Before

`CellInner` held each subscriber list as
`parking_lot::Mutex<Arc<Vec<(Uuid, Arc<Subscriber<T>>)>>>` with copy-on-write:

- `subscribe` — clone the entire `Vec`, push, `mem::replace`. **O(n) + alloc.**
- `unsubscribe` — `.iter().filter(|(i,_)| *i != id).cloned().collect()`.
  **O(n) scan + alloc.** This is the profiled `eq<Uuid>`.
- `notify` — clone the `Arc` (O(1)), drop lock, iterate. Optimal.

The design optimized notify (O(1) snapshot) at the cost of every mutation being
O(n). Under per-fire resubscription that O(n) mutation dominates.

### After — lazily-rebuilt snapshot over an id-keyed index

```rust
struct SubscriberRegistry<S> {
    index: FxHashMap<Uuid, Arc<S>>,          // authoritative; O(1) insert/remove
    snapshot: Arc<Vec<(Uuid, Arc<S>)>>,      // cached notify snapshot
    dirty: bool,                             // index changed since last snapshot
}
```

- `subscribe` — `index.insert(id, sub)`; set `dirty`. **O(1).**
- `unsubscribe` — `index.remove(&id)`; set `dirty`. **O(1). Scan gone.**
- `notify` — if `dirty`, rebuild `snapshot` from `index` **once** (O(n),
  amortized across all mutations since the last notify), clear `dirty`; then
  clone the `Arc` and iterate lock-free exactly as before.

**Cost change**

| path                       | before        | after                         |
|----------------------------|---------------|-------------------------------|
| subscribe                  | O(n) + alloc  | **O(1)**                      |
| unsubscribe                | O(n) scan     | **O(1)**                      |
| notify, subscriptions stable | O(1)        | O(1)                          |
| notify, after a change     | O(n) fanout   | O(n) rebuild + O(n) fanout    |

Pure churn (resubscribe every fire) still pays one O(n) snapshot rebuild per
fire — but it replaces **two** O(n) COW copies (subscribe + unsubscribe) with
**one** O(n) rebuild, and any subscribe/unsubscribe not immediately followed by
a notify is now free. Steady high-fanout (stable subs, e.g. the 9,200/sec
`ReportResponse` cell) is unchanged — still an O(1) snapshot grab.

**Dependency:** none. Uses `rustc_hash::FxHashMap`, already a workspace dep.
Rejected `im`/persistent maps: they'd give O(log n) mutation + O(1) snapshot,
but add a dependency to the hottest struct and per-node memory overhead across
all ~49k live cells while RSS is itself a problem. The std-only lazy-rebuild
kills the named O(n) scan with zero new deps and zero steady-state overhead.

### Invariants preserved (all load-bearing)

- **Drop-outside-lock.** Displaced `Arc`s (removed subscriber, replaced
  snapshot) drop *after* the mutex guard releases — a subscriber's drop can
  cascade into upstream `CellInner` drops that touch other cell mutexes;
  running that under our lock allowed two concurrently-dropping cells to
  acquire each other's mutex and deadlock. Registry methods return the
  to-be-dropped value; the caller drops it post-unlock.
- **Snapshot semantics.** A subscriber added during a notify lands in the next
  notify (it's inserted into `index`, sets `dirty`; the in-flight notify
  iterates its already-cloned snapshot). Unchanged.
- **At-most-one-late-notify under concurrency.** A subscriber whose guard drops
  concurrently with an in-flight notify may still receive that one notify (it's
  in the already-cloned snapshot). Identical to the previous COW behavior.
- **Reentrancy, metrics, trace, `result_subscribers`** all preserved; the two
  registries share the same `SubscriberRegistry<S>` type.

Iteration order changes from insertion order to `FxHashMap` order. No operator
or test depends on subscriber notification order (verified).

---

## Phase 2 — Reduce per-hop propagation cost (REFRAMED)

**Target:** the `drop<CellInner<…>>` + `atomic_sub` cascade (cost #2).

**Original framing (rejected by the code).** The plan assumed value cells were
rebuilt per animation frame and the fix was to hold a stable cell and update it
in place. Scoping against rship disproved this — the animation hot path is
*already* stable-cell + in-place:

- `execute_pure_node` (binding_node.rs) is
  `ctx.report(BindingNodeResolvedInputs).map(evaluate).materialize()` — one
  output cell per key, created once; per-frame input changes flow as in-place
  value updates + notify, never a rebuild.
- `value_track_sampler` `switch_map`s on **instance/lane topology** (deduped,
  0 re-fires per frame per rship's counters); the inner
  `join(keyframes, t).map(sample).materialize()` is built once per topology and
  only recomputes in place as `t` advances.

So there is no per-frame rebuild to eliminate. The per-frame
`drop<CellInner>`/`atomic_sub` is therefore **not teardown** — it's the
intrinsic cost of push propagation through the graph each frame:

1. Every operator/materialize callback does `weak.upgrade()` → `notify` →
   drop, i.e. an `Arc<CellInner>` refcount inc/dec **per hop per fire** (the
   `atomic_sub`). The `Weak` is load-bearing — it breaks the cell↔guard
   ownership cycle — so the upgrade can't simply be removed.
2. Each hop clones the value `Arc` (`signal.clone()`) and drops the previous
   one.

Multiplied by graph depth × node count × frame rate, that is the residual ~97%
after rship's own restructuring (which correctly hit diminishing returns — the
cost *moves* between cells but doesn't drop, because it's the primitive, not a
graph-shape bug).

**Addressable levers (in headroom order):**

- **Fewer hops per frame** — this is really Phase 3 (fusion), especially across
  *report boundaries*, each of which is a materialized cell. Fewer intermediate
  cells ⇒ fewer per-frame `weak.upgrade`/notify/value-clone cycles. Largest
  structural win, largest surface.
- **A cheaper per-hop propagation path** — restructure notify so a hop doesn't
  pay a full `Weak::upgrade` (atomic inc/dec) every fire, e.g. a
  dirty-mark/scheduler (pull) hybrid or a notify path that borrows the
  downstream cell without a transient strong `Arc`. Deep architectural change;
  spike only if Phase 1 + fusion leave propagation dominant.

**Gate:** land Phase 1 and take the fresh HRLV capture first. If the `eq<Uuid>`
bucket collapses under Phase 1, the dominant remaining cost was subscribe churn
(in the report/fanout layer, not the value operators — those don't re-subscribe
per frame), and this phase's priority drops. If it barely moves, propagation is
the floor and the levers above become primary.

---

## Phase 3 — Extend Pipeline fusion to collection operators (DESIGNED)

**Target:** the ~436-deep towers allocating an intermediate `Cell` per hop
(cost #3).

Pure scalar operators (`map`, `filter`, `tap`, `try_map`, …) **already fuse**
via the `Pipeline<T, S>` layer: a chain installs a single subscription on the
root running one composed closure, allocating no intermediate cells (see
`pipeline/mod.rs`). The remaining intermediate-cell allocation is the
**collection operators** — `join`, `group_by`, `select`, `project`, the
`*_join` family — which each `materialize` a stateful intermediate cell.

Direction: give the incremental collection operators a pipeline-style install
form so a chain of pure-ish collection ops collapses onto one materialized
sink instead of one cell per hop. This is the largest and most invasive phase
(the collection operators are stateful/incremental, unlike the fused scalar
ops) and should land operator-by-operator behind the existing test suite,
lowest-risk first.

---

## Sequencing rationale

Phase 1 is self-contained (one file, `cell.rs`), directly targets the single
largest frame-sample bucket, and preserves every documented concurrency
invariant — so it lands first and independently. Phases 2 and 3 change operator
semantics and want a fresh profile against HRLV (via nimble-walrus/rship) to
confirm how much of the 83 % remains once the `eq<Uuid>` scan is gone, before
committing to their larger surface area.
