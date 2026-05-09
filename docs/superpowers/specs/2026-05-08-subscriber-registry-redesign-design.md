# Subscriber Registry Redesign — Mutex<Arc<Vec>> over ArcSwap<Vec>

**Date:** 2026-05-08
**Status:** Approved (diagnosis); ready for implementation plan
**Related work:** `1895096` (defer dropping displaced subscriber list outside cell mutex) — fixed deadlock cascade but didn't address the per-swap base cost. This redesign addresses the cost.

## Goal

Replace `Cell::subscribers: ArcSwap<Vec<(Uuid, Arc<Subscriber<T>>)>> + subscribers_writer: Mutex<()>` (and the parallel `result_subscribers` pair) with `Mutex<Arc<Vec<(Uuid, Arc<Subscriber<T>>)>>>`. Cell-drop cascades stop paying arc-swap's slot-table walk per cell teardown.

## The Bottleneck

Captured in a 30-second cycles trace of `rship_server` under a scene-load workload (release+frame-pointers+debuginfo=2). 14,807 → 35,459 → 42,768 samples across baseline / load_full-patched / search-index-spec captures. In all three:

- **67–77 % of on-CPU samples land in `arc_swap::strategy::hybrid::wait_for_readers::{closure_env#0}<Arc<Vec<(Uuid, Arc<Subscriber<…>>)>>>`** — i.e. the spin loop inside `ArcSwap::swap` / `ArcSwap::store` operating on `Cell::subscribers` (and `result_subscribers`).
- **0 % of samples in `downcast_ref` / `TypeId` / `vtable`.** Dynamic-dispatch downcast cost is below the noise floor on this workload — a separate downstream A/B confirmed it.
- **5–7 multi-second off-CPU stalls per 30 s on a single `rship_server` thread**, sum ≥ 22 s of wait_time. Stacks `[unknown]` (sched events use shorter dwarf snapshots).

Reading a wait_for_readers stack to its root (verified post-`load_full` patch, where the on-CPU cost only dropped from 74.4 % → 67.6 %):

```
arc_swap::ArcSwap::swap                       ← wait_for_readers
  ← hyphae::pipeline::cell_impl::install::{closure}
  ← Box<dyn FnMut>::drop                       (closure being dropped)
  ← SubscriptionGuard::drop                    (subscription drops fire unsubscribe)
  ← drop<DashMap<Uuid, SubscriptionGuard>>     (cell's own outgoing-sub map drops)
  ← drop<CellInner<Arc<BindingNodeInputValueOutput>>>
  ← drop<Box<dyn Fn(&Signal<…>)>>              (downstream subscriber callback drops)
  ← drop<DashMap<Uuid, SubscriptionGuard>>     (another cell's sub map)
  ← drop<CellInner<Arc<dyn AnyOutput>>>
  ← drop<HashMap<Arc<str>, …>>                 (downstream session's subscription registry)
  ← drop<ClientSession<…>>                     (downstream WebSocket connection ending)
```

So `wait_for_readers` is being driven by **Cell drop cascades through SubscriptionGuard::drop**, not by concurrent subscribe-during-notify contention. Each `SubscriptionGuard::drop` calls `swap` on its source cell to remove its registration. Each swap pays the per-call slot-table walk inside `Debt::pay_all`: O(threads × ~8 slots), ≈ 200–400 atomic loads. Multiplied by thousands of cells in a teardown cascade, this is the multi-second hot loop.

The 1895096 fix prevented deadlock by deferring the displaced-Vec drop outside `subscribers_writer`, but didn't change the per-swap base cost — and that cost is the dominant work, regardless of whether anyone actually held a reader debt.

### Why `load_full()` in notify (the patch we tried) only moved 7 pp

`load_full()` releases the *reader's* debt slot during notify, helping the case where a concurrent `subscribe`'s `swap` would otherwise spin while a notify callback holds the slot. That isn't the dominant case. The dominant case is `swap`-on-drop, where there are no concurrent readers — the cost is the constant slot-walk, not the spin.

## Non-Goals

- **Drop-cost reduction on the four other per-Cell ArcSwap fields** (`name`, `error`, `slow_subscriber_threshold_ns`, `slow_subscriber_callback`). Each costs one extra `wait_for_readers` per Cell drop, but they are minor next to the subscribers Vec. Out of scope; consider as a follow-up.
- **Reducing the *count* of cells that drop in a cascade.** That's caller-side (rship binding-graph teardown shape). This spec only addresses per-cell cost.
- **Replacing the Vec-rebuild on subscribe/unsubscribe with a delta queue.** Strict subscribe-then-emit visibility is required: a subscribe completing before notify entry must be observed by that notify. CoW preserves that.
- **DashMap.** Prior measurement showed DashMap worse than the current pattern for this workload; not pursuing.

## Architecture

### Field shape

```rust
pub struct CellInner<T> {
    value: Mutex<Arc<T>>,                                                          // unchanged
    name: ArcSwap<Option<Arc<str>>>,                                               // unchanged (out of scope)
    error: ArcSwap<Option<Arc<anyhow::Error>>>,                                    // unchanged

    // BEFORE:
    //   subscribers: ArcSwap<Vec<(Uuid, Arc<Subscriber<T>>)>>,
    //   subscribers_writer: Mutex<()>,
    // AFTER:
    subscribers: parking_lot::Mutex<Arc<Vec<(Uuid, Arc<Subscriber<T>>)>>>,

    // Same replacement for result_subscribers.
    result_subscribers: parking_lot::Mutex<Arc<Vec<(Uuid, Arc<ResultSubscriber<T>>)>>>,

    slow_subscriber_threshold_ns: ArcSwap<Option<u64>>,                            // unchanged
    slow_subscriber_callback: ArcSwap<Option<SlowSubscriberCallback>>,             // unchanged
    completed: AtomicBool,
    errored: AtomicBool,
    metrics: Option<Arc<Metrics>>,
    id: Uuid,
}
```

The `subscribers_writer` and `result_subscribers_writer` mutexes are deleted — the new mutex serves both functions.

### Notify hot path

```rust
// BEFORE:
let subs = self.inner.subscribers.load_full();   // arc_swap::load + Guard::into_inner
for (_id, sub) in subs.iter() { (sub.callback)(&signal); }

// AFTER:
let subs = Arc::clone(&*self.inner.subscribers.lock());  // brief mutex, Arc bump, drop lock
for (_id, sub) in subs.iter() { (sub.callback)(&signal); }
```

`parking_lot::Mutex::lock()` on an uncontested lock is one cmpxchg + one ordinary store on unlock. The hold duration is exactly one Arc refcount increment — sub-microsecond. Subscriber callbacks run *outside* the lock, exactly as before.

Compared to today: notify trades one debt-slot acquire-release for one mutex acquire-release + one atomic refcount bump. The Arc bump is per-notify; today's debt-slot path is also per-notify. The lock acquire is the new cost — bounded, and avoids the constant-overhead slot-walk on the *write* side.

### Subscribe / Unsubscribe

```rust
fn subscribe(&self, callback: SubscriberCallback<T>) -> SubscriptionGuard {
    let id = Uuid::new_v4();
    let sub = Arc::new(Subscriber::new(callback));
    {
        let mut guard = self.inner.subscribers.lock();
        let mut next: Vec<_> = (**guard).clone();   // O(N) Vec clone, same as today
        next.push((id, sub));
        *guard = Arc::new(next);
        // old Arc<Vec> drops here on next iteration (out-of-scope reassignment)
        // and decrements its refcount — no wait_for_readers, no slot walk.
    }
    let cell = self.clone();
    SubscriptionGuard::new(id, source, move || {
        let mut guard = cell.inner.subscribers.lock();
        let prev_len = guard.len();
        let next: Vec<_> = (**guard).iter().filter(|(i, _)| *i != id).cloned().collect();
        if next.len() != prev_len {
            *guard = Arc::new(next);
        }
    })
}
```

The 1895096 swap-and-defer pattern collapses to "old Arc drops at end of locked block" naturally. We can't reach the deadlock the original commit fixed because we no longer hold a writer mutex *while* doing arc-swap operations — the only operation under the lock is a pointer assignment.

The cascade-through-Drop pattern still happens — when `next` is bound to `*guard`, the old `Arc<Vec>` becomes unreachable and its drop runs. Dropping the old Vec drops each `Arc<Subscriber<T>>`, which can recursively drop captured upstream Cells. That recursion is *outside* the lock now — same property as today, no regression.

### result_subscribers

Same shape, same change. `ResultSubscriber<T>::callback` returns `Result<(), String>`; otherwise structurally identical.

### Imperative `unsubscribe(id)` and `subscriber_count()`

`unsubscribe(id)`: same shape as the SubscriptionGuard's closure above — lock, filter-rebuild, swap.

`subscriber_count()`: read-side, takes the mutex briefly to read `guard.len()` (or `(**guard).len()` for the Arc<Vec> deref), drops the lock. O(1). Today this hit `subscribers.load().len() + result_subscribers.load().len()`; replacement is symmetric.

## Why `parking_lot::Mutex` over `std::sync::Mutex`

- Smaller (1 word vs 5+) — matters for `CellInner` which is allocated per Cell, and we're already paying for two of them.
- Faster uncontested fast path (single cmpxchg, no syscall).
- Fair contention (avoids starvation under heavy churn).
- No poisoning — we don't carry meaningful state across panic boundaries here, and avoiding poisoning matches the existing `value: Mutex<Arc<T>>` field's intent (where `expect("cell value poisoned")` is the only call site).

If hyphae has a policy against `parking_lot`, `std::sync::Mutex` works mechanically; lose ~20 % uncontested fast-path performance and add poisoning handling.

## Behavior preserved

| Property | Before | After |
|---|---|---|
| Subscribe completes before notify entry → that notify sees it | ✓ (swap returns before subscribe returns) | ✓ (Arc<Vec> assigned before lock release) |
| Notify reads a coherent snapshot (no torn iteration if subscribe races) | ✓ (load returns one Arc) | ✓ (lock+clone returns one Arc) |
| Subscriber callbacks run lock-free (no internal hyphae lock held during user code) | ✓ | ✓ |
| 1895096 deadlock fix's invariant — internal cell mutex never held during user-visible Drops | ✓ | ✓ (lock dropped before old Arc<Vec> drops) |
| Drop cascades don't deadlock | ✓ (post-1895096) | ✓ (no writer mutex left to deadlock on) |

## Cost comparison

Per-cell-drop:
- **Before:** 1 swap on `subscribers` + 1 swap on `result_subscribers` ≈ 2 × (slot_walk = ~200–400 atomic loads) ≈ 400–800 ns CPU. Multiplied by every cell in a cascade.
- **After:** 1 mutex drop + 2 Arc<Vec> drops + recursive Vec-element drops. Mutex drop: ~free. Arc drop: 1 atomic decrement + (if last) deallocation. No slot walks.

Per subscribe / unsubscribe:
- **Before:** lock subscribers_writer + load + clone Vec + push + swap (slot_walk) + drop old Arc + unlock. The slot walk dominates under contention.
- **After:** lock subscribers + clone Vec + push + Arc::new + assign + unlock. No slot walk, no wait spin.

Per notify:
- **Before:** load (debt slot acquire — ~1 atomic) + iterate (callbacks lock-free) + Guard drop (debt slot release).
- **After:** lock + Arc::clone (1 atomic refcount bump) + unlock + iterate (callbacks lock-free) + Arc drop (1 atomic refcount decrement).

The notify hot path is comparable cost (one cmpxchg added, one atomic added; debt slot machinery removed). The win is on the write/drop side, which is where the trace shows the bottleneck.

## Risk: notify-time mutex contention

A heavily-notified cell (high-frequency value updates) on which subscribers are also being added/removed will see mutex contention. Worst case: writers wait while a notify holds the lock for ~1 µs (the Arc clone). Compared to today's writer Mutex + arc-swap slot walk, contention is shorter and sleep-free in the uncontested case.

If contention proves measurable in benchmarks: the `Arc<Vec>` snapshot pattern lets us layer in lock-free reads later (e.g., `arc_swap::ArcSwap<Arc<Vec>>` for the read side, with the mutex retained for writer serialization) without a third redesign — but only if measured to be needed.

## Validation plan

1. **Unit / concurrency stress tests** in `hyphae/tests/` exist for the existing subscriber path. Re-run unchanged. Expect green.
2. **Benchmark harness** (mirror the cell-map-query-plans bench shape from `2026-04-25-cell-map-query-plans-bench-results.md`):
   - Cell teardown cost: build a graph of N=10,000 cells with K=8 subscribers each, drop the root, time to full quiescence. Expect ≥ 5× improvement.
   - Notify rate: cell with K=10/100/1000 subscribers, 100k notifies, no concurrent mutation. Expect within ±5 % of current.
   - Subscribe storm: K=1000 concurrent subscribers on one cell, time to all-subscribed. Expect ≥ 3× improvement.
3. **rship trace re-capture** under the same scene-load workload: 30 s perf-server.sh + perf-server-sched.sh. Expected:
   - `wait_for_readers` prevalence drops from 67 % → < 5 %.
   - On-CPU sample volume drops (work that was spinning is no longer present).
   - Multi-second off-CPU stalls drop materially (writer mutex queue dissolves; the new mutex's contention is sub-µs).
4. **Manual smoke** — open / close several scenes back-to-back, observe absence of multi-second hangs.

## Migration

Single PR scope. Touched files:
- `hyphae/src/cell.rs` — field replacements, notify hot path, subscribe / unsubscribe, SubscriptionGuard::drop closures, imperative `unsubscribe(id)`, `subscriber_count()`.
- `hyphae/src/lib.rs` — re-export change if `Mutex` was re-exported from `cell` (unlikely).
- `hyphae/Cargo.toml` — add `parking_lot` (already a transitive dep via dashmap).

No public API change. No behavior change for consumers.

## Open questions

1. **Should we also collapse the four other per-Cell ArcSwaps?** Each contributes one `wait_for_readers` per Cell drop. Combined ≈ 4 × per-cell drop slot-walks today. Worth the cleanup, but separable and lower-priority than the subscribers fix.
2. **Should the mutex hold the `Arc<Vec>` directly, or a `Vec` with a separate `Arc<…>` snapshot kept lazy?** The Arc<Vec> form is simpler and avoids a per-notify Vec clone. Lazy snapshot would only help if mutex-hold-time becomes a problem — measure first.
3. **Should `parking_lot` be made a direct dep, or re-export through an existing module?** Trivial; defer to whatever hyphae's policy is.
