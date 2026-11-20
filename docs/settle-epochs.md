# Settle epochs (design, not implemented)

This document designs a pipelined settle. Every event set gets a ticket in
arrival order, and each node settles tickets in increasing order. Ticket N+1
can settle at low heights while ticket N is still settling above it. There is
no settle lock, no run, and no cone reservation.

The model checkers are in `docs/settle-epochs-model/`. They explore every
interleaving of the rules below. Section 9 lists what it proves
and at what bounds.

## 1. Why the earlier options failed

Option 1 (`rtt-opt1`, section 13 of `settle-core.md`) reserved a cone of nodes
per run at admission and walked `node_dependents` under one lock. Option 2 used
a central holder with overtake rules. Both assumed that a write's reach can be
predicted up front. It cannot: 86% of opt1's cone escapes were a cascade from a
few real misses, and the misses were callback writes to unowned cells,
collection mutations inside a settle, and `switch_map` rewires.

This design predicts nothing. A node learns what it needs from its own inputs,
when it needs it.

## 2. Vocabulary

- **Ticket.** A `u64` taken at admission, in arrival order. One per event set.
  An `Epoch` stays a tag that rides along; it never splits anything.
- **Edge.** `(src, dst, from, to)`. `dst` hears `src` for tickets in
  `[from, to)`. A static edge has `from = 0, to = ∞`.
- **Seeded-through `S`.** Every ticket below `S` has had its writes placed in
  pending lists. Only the admitter writes it.
- **Processed `p(n)`.** The last ticket node `n` fired.
- **Watermark `w(n)`.** A promise: `n` will never again deliver a ticket below
  `w(n)`. Monotone, cached per node.

## 3. The per-node rule

A node keeps pending deliveries keyed by ticket. It fires its lowest pending
ticket `t` when its input watermark is past `t`:

```
contrib(e)  = let lo = max(e.from, w(e.src)); if lo >= e.to { ∞ } else { lo }
in_w(n)     = min(S, min over in-edges e of contrib(e))
fire(n, t)  requires t == min_pending(n) and t < in_w(n)
w(n)       := max(w(n), min(in_w(n), min_pending(n)))      // refresh
```

Firing `t` computes the node's value from its inputs' values at version `t`,
records `(t, value)`, sets `p(n) = t`, and pushes `t` to every out-edge active
at `t`. One fire is atomic with respect to the node.

That `in_w(n) > t` holds means every input has delivered everything it will
ever deliver for tickets up to `t`. Glitch freedom (G1, G5) then holds without
heights: a join fires `t` once, after every input is final for `t`.

### Where a callback runs

A hyphae operator keeps its state in the callbacks it subscribes to its
inputs (a join's latest left and right, `switch_map`'s generation). If an
input's callback ran when that input fired, input A's callback for ticket
`t+1` could run before input B's callback for `t`, and B would push a value
built from A at `t+1` into ticket `t`.

**Rule: a subscription owned by a node runs its callback at that node's
fire.** When `a` fires `t`, a subscriber whose guard node `c` owns (bound
by `own_keyed`) does not run. Its callback and the signal become a pending
delivery of `c` at `t`. When `c` fires `t`, it runs its deliveries in stamp
order, and a write a callback makes to `c` itself is `c`'s value at `t`.
Operator state therefore lives with the node it computes, and it sees its
inputs in ticket order. A subscription that no node owns runs at its
source's fire, and its writes follow the routing rules of section 6.

An owned subscription that cannot be bound (its guard does not name the
subscription) still runs at its source's fire. That is a correctness hole,
counted per owner site in `SettleStats::unbound_owned`.

## 4. How a node learns that earlier tickets cannot reach it

### Why the pull walk was replaced

The first version pulled a silent input's watermark by walking its ancestors.
The shadow measurement on rship setup traffic (`epoch-shadow` 7466369,
`settle/shadow.rs`, 6 runs, 633 s) failed the gate:

- 47.8% of fires needed a walk, at 0.62 walks per fire.
- 99.6% of joins hit a silent input.
- A walk visited 7 nodes at p50, 80 at p99, and 2,560 at most. It cost 2 µs at
  p50 and 49 µs at p99.
- 64.8% of walks ran all the way to `S`.
- The total was about 175 node reads, roughly 84 µs, per ticket of about 21
  fires.

Memoization failed because a quiet input's cached watermark is a number, and
`S` moves past it on every ticket.

### Quiet certificates

Most walks proved one fact: nothing in the input's ancestry is pending. That
fact changes only when a ticket is **seeded** into the ancestry. Pending work
spreads only along edges, so it can enter an ancestry from outside only at a
seed. A push from `y` to `z` means `y` was already pending, and `y` is an
ancestor of everything `z` is.

So each node carries a mark: `NONE`, `TENTATIVE`, or `QUIET`.

- **Meaning.** `QUIET(n)` promises that no node in `n`'s ancestry, `n`
  included, holds pending work below `S`. The set of `QUIET` nodes is closed
  upward: a node is `QUIET` only if all its inputs are.
- **Read (the hot path).** A join reads a quiet input's watermark as `S`: one
  atomic load of the mark, and `S`, which it already holds. This is O(1), with
  no walk, and it stays valid across any number of tickets that do not touch
  the ancestry.
- **Invalidate (at seeding).** Admitting ticket `t` at node `x` pushes `t` into
  `x`, sets `x` to `NONE`, and walks `x`'s descendants, clearing every
  non-`NONE` mark. It stops at `NONE` nodes, because by closure nothing below
  them is marked. `S` passes `t` only after the walk ends. A reader that sees
  `S > t` therefore sees the cleared marks.
- **Validate (on demand).** When a join finds a silent input not `QUIET`, it
  marks the input's not-yet-`QUIET` ancestors bottom-up. For each node it does
  four steps:
  1. Check that the node has no pending work and that every input is `QUIET`.
  2. Write `TENTATIVE`.
  3. Re-check the same conditions.
  4. `compare_exchange(TENTATIVE, QUIET)`.

  An invalidation clears `TENTATIVE` too, so a seed that lands anywhere in
  between makes the exchange fail. Readers trust only `QUIET`.
- **Fallback.** A node that is not `QUIET` uses its numeric watermark. A fire
  refreshes the firing node's watermark from its inputs at O(inputs), so a
  busy chain keeps its numbers fresh as it fires. A real blocker is parked on,
  as before.

**Cost per fire:** O(1) for each `QUIET` input, O(1) for an input that just
delivered, and O(inputs) for the refresh. **Walks:** validation visits only
nodes that are not yet `QUIET`. Each such node was cleared by a seed, and each
clear follows at most one validation. So the total walk work is at most twice
the number of (seed, previously validated descendant) pairs. Cost is paid per
seed into an ancestry, not per fire.

**The rship pattern** is joins whose other input is quiet for a long time. The
quiet side is validated once, then read in O(1) on every later fire until
something is seeded into its ancestry. Nothing global is read except `S`.
Nothing waits on unrelated tickets, because the mark is exact for the node's
own ancestry.

**What is still unmeasured, and is the next gate:**

1. **Seed hit rate.** How often a seed lands in a quiet input's ancestry
   between two checks. A store collection written every ticket that sits
   above many joins would re-clear and re-validate them on every write. The
   shadow can count, for each validation, whether a seed landed in its
   ancestry since the previous one.
2. **Invalidation length on the admission path.** `S` cannot pass the ticket
   until its walk ends. A long walk therefore holds back every later ticket,
   though it clears only nodes that were validated since the last seed there.

**Leads considered and rejected:**

- **A per-height or per-region frontier.** A node at height h waits for every
  earlier ticket to clear all heights below h, including tickets that cannot
  reach it. That puts a probe behind the timer tick again.
- **Announcing every pending ticket downstream.** Announcing along the edges a
  ticket takes says nothing about silent inputs. Announcing along every static
  edge is a cone walk per ticket. Quiet certificates announce only at seeds,
  and only to marked nodes.

**Promise safety, the one invariant everything rests on:** a node never
receives a ticket below its watermark or at or below `p(n)`. The checker
asserts it on every push. It holds because:

1. A delivering input had `w ≤ t` until it fired `t`, so the target could not
   have promised past `t`.
2. A seeded write gets ticket `S`, and every watermark is at most `S`.
3. An edge added by the target's own fire (switch_map, section 6) starts at
   `t+1` while the target's watermark is at most `t`, because it still held `t`
   when the fire began. This keeps order, but section 6 shows it still breaks
   completion, so the design does not use it. Structural edges are published
   with `S` instead.

## 5. Versioned reads

A node keeps `(ticket, value)` entries. `value_at(n, t)` is the newest entry
at or below `t`. A fire reads its inputs at its own ticket.

- **An input read** is always ready, by the fire rule.
- **A side read** with `get()` of a node `x` that is not an input reads
  `value_at(x, t)` and must wait until `w(x) > t`. The checker models this as a
  guard. Removing that guard breaks serializability (section 9). A thread that
  waits must help: it fires the work that it waits on, which is at a lower
  ticket or a lower height, so the wait-for order is strictly decreasing and
  cannot cycle. A bounded pool that parks instead can starve.
- **Pruning.** Keep the entries at or above the newest entry at or below
  `C`, the completed-through ticket (section 7). Under steady load this is one
  or two entries per node. A node that a long-lived reader at an old ticket
  holds keeps more until that reader ends.

Recommendation: drop side reads (section 10). Without them, a fire reads only
its inputs, the wait disappears, and history is needed only for inputs.

## 6. Dynamic edges, and the switch_map replacement

**Rule: the graph that carries watermarks is static between structural
tickets.** Every edge a node can ever hear on exists before any of its
sources can fire a ticket that would travel it. Data selects among inputs.
Edges do not appear mid-ticket.

### switch_map as a static selector

`switch_map` builds no nodes. Per ticket it yields exactly one of:

- **(a) a whole source.** `selector.switch_map((src1, src2, src3), pick)`.
  The edges `selector → out` and `srcN → out` are static. At ticket N,
  `pick(selector@N)` names one source, and the output is that source's value
  at N.
- **(b) a key into a collection.** `key.lookup(table)`. The edges
  `key → out` and `table → out` are static. At ticket N the output is
  `table@N[key@N]`.

Reading every input at version N is the whole mechanism. Nothing subscribes or
unsubscribes when the selection changes.

Semantics, for both forms:

1. When the selector or key changes at N, the output at N is the newly
   selected source's value at N, or `table@N[new key]`. There is no glitch:
   the output fires once at N, after both inputs are final for N.
2. When the selected source or the entry under the current key changes at N,
   the output updates at N.
3. From N on, changes to the old source or the old key do not reach the
   output. A change at N itself to the old key, made in the same event set as
   the key change, is not seen either.
4. The output emits nothing on a ticket that changes only unselected sources
   or other keys.

**Per-key fan-out.** So that a change to key k wakes only the readers of k,
the table routes a change at ticket t to reader R only if R's key at t is k.
The table learns R's key at t from R's key cell, and only once that cell's
watermark is past t. Until then it pushes to R anyway, and R filters. Routing
by the key R last fired with is wrong. The checker finds it at four tickets:
the key changes to 1 at ticket 0, entry 1 changes at ticket 1, and the table
fires ticket 1 before R has fired ticket 0, so the update is lost. The
watermark rule is correct in the same ticket (key and entry change together:
R gets the ticket from the key cell either way) and in adjacent tickets.

The cost of the static form is fan-in: without routing, a reader hears every
ticket that changes the table. With routing it hears its own key, plus any
ticket that its key cell has not yet passed.

### Target API: lookup into a family

```rust
fn lookup<F: Family>(self: Cell<F::Key>, family: F) -> Cell<F::Output>;
fn lookup<F: Family>(self: Cell<Option<F::Key>>, family: F) -> Cell<Option<F::Output>>; // None in, None out
```

```rust
let target_id: Cell<Option<TargetId>> =
    emitter.map(|e| e.as_ref().map(|e| e.target_id.clone()));
let target = target_id.lookup(ctx.family::<GetTargetById>());
```

A family's output is one value (a myko report) or a keyed collection (a myko
view). The two differ only in output shape. The lookup node has two static
edges: one from the key cell, and one from the family. A join over a reactive
key set (P4) is the same lookup with a set-valued key, over a view family.

At the ticket N where the key changes, the output is `family[new key]@N`:

- **A value** is that row's value at N.
- **A collection** is told the new key's snapshot as one `Initial` at N, and
  not a diff from the old key's rows. After N it gets `Delta`s of the new
  key's rows only. A row change in ticket N itself is already in the snapshot
  and is not told again.

### Families (P3) and joins over a key set (P4)

A `Family<R>` is a parameterized report exposed as a keyed collection. Its
rows are built when a key is first demanded, refcounted, and torn down when no
reader remains. This is growth at a keyed boundary: new nodes whose inputs are
existing nodes. Nothing rewires an existing node's inputs.

**The anchor rule.** A family node `Fam` has static edges from every input its
rows can read (for a myko report, the store collections it queries) and from
the demand: the key cells, or the key-set cells, of its readers. Readers have
static edges from their key cell and from `Fam`.

**Build inside the ticket.** When `Fam` fires ticket N and finds a demanded
key k with no row:

1. It builds row node `F_k`, whose value at N is computed from the inputs at N.
2. It sets `p(F_k) = N` and `w(F_k) = N+1`.
3. It adds the edges `input → F_k` and `F_k → Fam` from N+1.
4. It backfills into `F_k` every ticket above N that the inputs have already
   fired.

The reader then sees `family[k]@N` at N. There is no lag and no structural
ticket.

This is sound, unlike a mid-ticket rewire of an existing node, because of the
anchor edge. Any ticket above N that an input has fired is also pending at
`Fam`, since `Fam` has a static edge from that input and is still firing N. So
no ticket that the backfill touches can have completed. The rewire
counterexample in section 6 had no such anchor. The checker confirms it:
without the anchor edge, ticket 1 reaches the new row after it completed.

**Teardown.** `Fam` drops `F_k` in the ticket where the demand for k drops,
inside its own fire. At that point `F_k` has passed the ticket, because `Fam`'s
fire waits on it, and `Fam` is its only consumer. The checker also passes when
the reader drops the row at its own fire and `Fam` rebuilds it on the next
demand. That is safe only because a row there is a pure function of its inputs
and a rebuild reproduces it exactly. A row with state of its own, such as a
fold, must be torn down by the family at the dropping ticket, or never.
Keeping a row alive is always correct; it only wastes work.

**Both output shapes.** The checker runs once with value rows (combined by
sum) and once with collection rows (sets, combined by union). For collections
the reader's diff stream is replayed the way a subscriber holds it. Every
insert must name an absent element, and every remove a present one. The
replayed state must equal the serial result at every emission.

**Joins over a reactive key set (P4).** The demand is a set. `Fam` builds or
drops rows for keys entering or leaving the set at ticket N, and the reader
combines the rows of the set at N. A singleton set is the lookup form (b).

### Rewiring at a ticket boundary (for anything select cannot express)

If a candidate set is truly not static, the rewire is its own **structural
ticket** `T`, taken at admission like any write. The admitter publishes the
edge change together with `S = T`. No node can have fired `T` yet, so nothing
needs backfill and per-ticket completion stays exact. Between the switching
ticket and `T` the output keeps following the old inner. At `T` it emits the
new inner's value at `T`.

### What the checker found about mid-ticket rewiring

Rewiring inside the switching ticket (`from = t+1`, with backfill of tickets
the new inner already fired, and retraction of the old inner's pending
entries) keeps per-node order and serializability. It breaks **completion**.
Counterexample, found at 2 tickets: ticket 0 flips the selector to S3. Ticket
1 writes S3 and completes. Then W's fire at 0 attaches S3 and backfills ticket
1 into W, after ticket 1's writer was told it was settled. Per-ticket
completion cannot see an edge that will be attached later to a node that is
behind. Fixing that needs a global check of every lagging dynamic node. So
mid-ticket rewiring is out.

### The other arrivals

| Arrival | Sound by construction? | Rule |
| --- | --- | --- |
| A write along a declared edge (operators, `own`, `own_keyed`, `switch_map` sources) | Yes | Section 3 |
| A map-query or `map_values` row | Yes, as data | Rows are entries inside one collection node with static edges, not nodes of their own. A row that must be its own node is created at a structural ticket |
| A callback write to a cell with no edge from the firing node (cases 2 and 3 of the opt1 escape analysis) | **No** | The target may already have promised past `t`, or processed `t+1`. Example: a callback in ticket N writes cell C. C has no edge from the firing node, so C's watermark depended only on `S`. C can have processed N+1, and C's dependents can have fired N+1 using C's promise. No local fact makes joining N safe. The write always becomes a new ticket |

**Declaring the internal edges** (`cell_map.rs:214`, the derived-collection
diff and length cells, myko projection cells) is **required to keep today's
semantics**: with the edge those writes join ticket N, as they do now. Without
it they are still correct, but they land one ticket later, and a join
downstream sees an extra intermediate state. Safety does not need it.

### Coverage of today's uses

A separate audit maps every `switch_map`, `flat_map`, and dynamic-graph site in
myko and rship onto (a) and (b), and proposes a static form for each site that
fits neither. This document does not repeat it.

## 7. Waiters, flush, Barrier, Exclusive

- **Completion.** Ticket `t` is complete when it is seeded and nothing holds a
  pending entry for it. The implementation is a per-ticket count, raised on a
  push and lowered on a fire, batched per worker. It is touched only by that
  ticket's own work. `C`, the completed-through prefix, advances over a ring of
  completion bits once per ticket. This is exact only because the
  watermark graph is static between structural tickets (section 6).
- **Read-your-writes.** A blocking writer waits for its own ticket only. When
  it returns, every node its write reached reflects it. It does not wait for
  earlier tickets that its write never touched. A probe therefore does not wait
  for the timer tick, unless they share nodes, and then only for the tick's
  work on those nodes. This is the latency win.
- **Feedback.** A writer is **not** owed its feedback tickets. A callback's
  write is a new arrival, and its ticket owes nothing to the writer that caused
  it. This bounds every wait by one ticket's work and removes the waiter
  inheritance that made three writers wait about 39 s in option 2. A caller
  that needs the whole chain uses `Epoch::scoped`, which already exists and
  does not block, or `flush`.
- **flush.** It takes a ticket with no writes and waits for `C` to pass it.
- **Barrier.** The admitter does not seed the barrier until `C` reaches it, and
  seeds nothing after it until the barrier completes. Only the admitter pays.
  The checker includes it.
- **Exclusive keys.** Subsumed. Every ticket is delivered on its own. Two emits
  never share a delivery.
- **Per-ticket cost (option 2's lesson 1).** There is no run and no setup per
  ticket. A ticket costs one admission (`fetch_add`, seed, publish `S`), one
  pending entry per node it reaches, and one completion count. Many small
  tickets cost what one large ticket costs per node fire. An epoch tag splits
  nothing.

**Coalescing (G3).** Today `Latest` drops values within a run. Here a `Latest`
node holding several ready tickets may deliver only the newest, tagged with the
highest ticket. That is the same allowance as G3 today. It is not in the
checker, which delivers every ticket.

## 8. Data layout

- **Slab of node slots**, `u32` ids, struct-of-arrays: `w: [AtomicU64]`,
  `p: [AtomicU64]`, `min_pending: [AtomicU64]`, `wake: [AtomicPtr]`. The fire
  check reads three cache lines.
- **In-edges inline**, as `SmallVec<[u32; 2]>`. Most nodes have one input, and
  the one-input case needs no walk. Edges with a finite interval exist only
  across a structural ticket and go to a side table behind a flag bit.
- **Height buckets** for the ready queue, one per height, popped lowest
  first. Heights now matter only for locality and for side-read ordering;
  correctness comes from watermarks. A rewire still maintains heights through
  `invalidate_height_cone`. That is a non-local walk, as it is today.
- **Pending** is a small vector of `(ticket, value)` sorted by ticket. Each
  fire takes the front run of entries with one ticket.
- **History** is a two-entry inline ring that spills to a vector only while an
  old reader pins it.

## 9. What the model proves

Two exhaustive state-space checkers in Rust, std only, in
`docs/settle-epochs-model/`. Each explores every interleaving of admission,
watermark refresh, and node fires. A fire is one atomic step of one node.
`run_model.sh MODES K [MUTANT]` runs the first one under
`systemd-run --user --scope -p MemoryMax=16G`.

**`epoch_model.rs`.** Sources S1, S2, S3. A = 10·S1. U is written by A's
callback, through a declared edge or as a fresh ticket. J joins A and S2 and
side-reads U. W selects S1 or S3 by S2's parity. Bounds: 3 tickets, each
writing S1, S2, or S3, so 27 sequences. A barrier at every position or none.
A→U declared and undeclared. W in three modes:

| W mode | Result |
| --- | --- |
| `lookup`: static edges to S2, S1, S3. This is form (a) | all properties hold, 5,435,959 states, 54 s |
| `next`: the rewire is a structural ticket | all hold at 2 tickets, 791,770 states, 7 s. A 3-ticket run passed 32 GB and was stopped |
| `now`: rewire inside the switching ticket, with backfill and retraction | order and serializability hold (13,038,578 states across `now` and `lookup`, with the completion check off for W). **Completion fails** with the check on: section 6 |

**`keyed_model.rs`.** Form (b): key cell K, collection C with keys 0 and 1,
and output L = C[K], with per-key routing. Every sequence of 5 tickets, each
writing the key, an entry, or both in one event set: 7,776 sequences,
13,352,217 states, 23 s. This covers a key and an entry changing in the same
ticket and in adjacent tickets.

**`family_model.rs`.** Store I, demand K (a set over 2 keys: empty, {0},
{1}, {0,1}), family Fam with built-in-ticket rows F0 and F1, and reader L =
the sum of the demanded rows. Every 4-ticket sequence of store writes and
demand changes: 625 sequences, 6,743,447 states, 34 s with value rows and 38 s
with collection rows. This covers a first
demand, a drop, a rebuild after a drop, and store writes in flight during a
build. A 5-ticket run (3,125 sequences) was still going after 15 minutes at about 100 MB and was stopped; it found no failure before it was stopped.

**`quiet_model.rs`.** It checks quiet certificates. Sources X, Y and Z feed
P = X, B = P + 10·Y, and the join C = Z + 1000·B. B is C's quiet input. The
seeder (seed, then one invalidation step per action, then publish `S`) and the
validator (check, write `TENTATIVE`, re-check, compare-exchange, one action
each) interleave with each other and with the fires. Every 3-ticket sequence:
27 sequences, 6,699,249 states, 60 s. Extra checks:

- In every state, a `QUIET` node has no pending work below `S` in its
  ancestry.
- No push lands in a `QUIET` node.
- From every reachable state, some path completes. Validation can fail and
  retry, so progress here is checked under fairness rather than by
  acyclicity.

Checked in all of them:

1. **Per-node ticket order.** Every fire is above `p(n)`.
2. **Promise safety.** No push lands at or below `p` or below the target's
   watermark, and no ticket reaches a node after it completed.
3. **Serializability.** At each terminal state, each node's history of
   `(ticket, value)` equals a one-ticket-at-a-time settle of the final ticket
   sequence, including fresh tickets. This covers the join J, the side read of
   U, form (a), and form (b), including which tickets the output emits on.
4. **No deadlock.** Every terminal state has every ticket seeded and nothing
   pending.
5. **Progress.** The reachable state graph is acyclic, so every path is finite
   and ends complete. Every ticket completes on every interleaving, with no
   fairness assumption.

Mutation check. Each rule was disabled in turn, and the checker failed every
time:

| Disabled rule | Failure |
| --- | --- |
| The input-watermark guard on fire | late arrival: W gets t0 after t1 |
| The watermark is capped by the node's own pending | broken promise: A gets t0 at watermark 1 |
| An undeclared callback write joins the ticket | broken promise: U gets t0 at watermark 1 |
| The side-read wait | not serializable |
| (`now` mode) the wait for the new inner, backfill, or retraction | not serializable |
| (`next` mode) the rewire is applied at W's fire, not at admission | not serializable |
| (keyed) routing by the reader's last fired key | not serializable: the update at ticket 1 is lost |
| (keyed) the output emits on every fire | not serializable: extra emissions |
| (family) no backfill into a new row | not serializable: store updates after the build are lost |
| (family) no anchor edge from the store to Fam | ticket 1 reaches the new row after it completed |
| (family, collection) a row diff is also told at the switch ticket | the diff stream double-tells ticket 0 |
| (quiet) `S` is published before invalidation ends | node P is quiet while X holds ticket 1 |
| (quiet) invalidation clears only direct dependents | node B is quiet while X holds ticket 1 |
| (quiet) validation writes `QUIET` with no re-check | X is quiet while it holds ticket 1 |
| (quiet) the promote is a blind store, not a compare-exchange | X is quiet while it holds ticket 1 |
| (quiet) validation ignores the inputs' marks | P is quiet while X holds ticket 1 |

Two family variants pass, and both are correct: keeping every row forever, and
dropping a row when the reader fires (sound only for stateless rows).

Not modeled: a bounded worker pool (the help-on-wait rule), `Latest`
coalescing, more than two keys, and collections with structural diffs
(insert or remove).

## 10. API changes

The user ruled that an API that blocks the design may change. Counts are from
`rg` over this workspace's myko and rship trees. Raw counts include unrelated
methods with the same name and are upper bounds. The closure scans are a
heuristic that misses nested braces, so they are lower bounds.

| API | Change | myko | rship |
| --- | --- | --- | --- |
| A callback write to an unowned cell | Always a new, later ticket. Never joins | 6 seen by the closure scan; 68 `.subscribe(` sites to audit | 6 seen; 141 sites |
| Internal derived writes (`cell_map.rs:214`, collection diff/len, projections) | Declare the edge where they are built, in hyphae and myko. This keeps same-ticket delivery | inside hyphae, plus myko projections (unmeasured) | none expected |
| `get()` inside a callback | Recommended: forbid it, and pass inputs explicitly. Otherwise it is a versioned read at the callback's ticket, and the reading thread helps the node it waits on | 0 seen by the scan; 335 raw `.get()` | 3 in subscribe and 8 in operator closures seen; 4,299 raw |
| `switch_map` | A static selector: form (a), a whole source, or form (b), a key into a collection. The closure builds no nodes (section 6). Per-site mapping is in the separate audit | 41 raw | 199 raw |
| Read-your-writes covers feedback | Covers the writer's own ticket only. Use `Epoch::scoped` or `flush` for the chain | every waiting write; behavioral | same |
| `SettleKey::Exclusive` | Remove. Subsumed | 1 | 1 |
| `SettleKey::Barrier` | Keep, implemented at the admitter | 8 | 0 |
| `flush` | Keep: wait for `C` to pass its ticket | 14 | 19 |
| `submit`, `enqueue` | Keep. A writer returns after admission | 29 raw | 41 / 23 raw |
| `batch`, `batch_with` | Keep. One event set is one ticket | 71 | 144 |
| Concurrent callbacks of different nodes | Already ruled (section 12 of `settle-core.md`) | | |

## 11. The guarantee table under epochs

| Guarantee | Under epochs |
| --- | --- |
| G1, G5 glitch-free joins | Hold, from watermarks rather than heights. A join fires ticket N once, after every input is final for N |
| G2 batch atomicity | Holds. One event set is one ticket, seeded in one step |
| G3 last-write-wins | Narrower. Coalescing happens only at a `Latest` node holding several ready tickets |
| G4 per-node order | Holds, and is proved |
| G6 replay | Holds. A subscribe replays a versioned read at the subscribing ticket |
| G7 gap-free diffs | Holds for a mutation along a declared edge. A mutation from a callback becomes its own ticket |
| G8 terminal order | Holds. A terminal is a pending entry and keeps its ticket order |
| G9 convergence | Holds, and is stronger: the result equals a serial settle of the ticket log |
| Read-your-writes including feedback | **Changes.** It covers the writer's own ticket only |
| "Every earlier write has settled when mine returns" | **Goes away** for writers. `flush` provides it |
| An undeclared callback write joins the current settle when the target is higher | **Goes away.** It becomes a later ticket |
| Exclusive keys | **Go away.** Subsumed |
| One callback at a time per node | Holds. One node fires one ticket at a time |

## 12. Pure callbacks (the coordinator's hypothesis)

If callbacks do no side reads and every write they make becomes a later
ticket, then a node's value at N is a pure function of its inputs at N, and
the graph is a function of the ticket log. That is what this design already
computes. The checker's serial reference is exactly that function.

Purity removes the side-read wait (the only blocking wait inside a fire),
callback-write coupling, and the need for history on non-input nodes. It does
not remove the watermark. "Computed up to N" still means that no ticket at or
below N is still in flight upstream, and in a push system some mechanism must
say so. Salsa answers it by pulling on demand, and differential dataflow
answers it with frontiers. Section 4 is the pull answer with memoized
watermarks.

One caution: "latest version at or below N" is enough for state (`Cell`,
`Latest`). It is not enough for events (`Source`, `merge`, `Each`), which must
see every ticket's delivery, not just the newest. Pending lists keyed by
ticket keep that.

Cost: one or two history entries per input node under steady load, pruned
below `C`. Pruning keys on the oldest incomplete ticket, which a reader at
that ticket holds open.

## 13. Implementation sequence

Each step ends in a state that a test or the harness can check. The sizes are
estimates.

1. **`Family` and `lookup`** (about 900 lines, plus migration). Add the
   family primitive with in-ticket row build and family-owned teardown, the
   value and collection output shapes, and `lookup` beside the current
   `switch_map`. Migrate sites per the audit. Check: the operator tests, and rship's suites, still on today's
   core.
2. **Declare the internal edges** (about 150 lines): `cell_map.rs:214`, the
   collection diff and length cells, and myko projections. Check: opt1's
   escape hook reports no internal escapes.
3. **Shadow tickets** (done: `epoch-shadow` 7466369). The pull walk failed
   its gate. Next, add quiet certificates to the shadow and measure the seed
   hit rate and invalidation lengths (section 4). This is the new gate.
4. **Pending lists keyed by ticket, and two-entry history** (about 500 lines)
   in `CellInner` and `SourceInner`. Check: the hyphae suite.
5. **The fire rule, parking and waking, and per-ticket completion** (about
   1,200 lines). This replaces the settle lock, runs, stints, and the
   frontier. Check: a loom test for park-then-recheck, and the suite under
   `WORKERS=8`.
6. **Undeclared writes become tickets; waiters own their ticket only**
   (about 300 lines). Remove Exclusive keys. Check: the waiter tests rewritten
   for the new read-your-writes, and the harness RTT and longest wait.
7. **Delete** the run, stint, hand-over, and cone code (about 1,500 lines
   removed).

Net in hyphae: about 3,000 lines added and 1,500 removed, plus the
switch_map migration in rship.
