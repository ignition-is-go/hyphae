# hyphae from first principles

Status: design proposal, not implemented. Branch `h4-fp`, written against
hyphae `h4` at `27436a9`, myko `h4` at `4f133980`, and the rship `h4` worktree.

This document says what hyphae should be and guarantee, which published
results each guarantee rests on, and how to find out cheaply whether the
proposal is wrong. It tests one hypothesis: that the root mistake is building
one graph node per datum, and that the fix is a small static graph of operators
over collections.

Every claim carries a label. *Measured* means a number read from an existing
run or census. *Read* means read in source or in a paper. *Inferred* means
reasoned from those. *Estimate* means a guess with its arithmetic shown.

## 1. Verdict on the hypothesis

The hypothesis holds, with five amendments.

**Supported.** The load-test server graph has 252,884 nodes created by 127
source locations (measured, section 2). The program is a few hundred
operators. The graph is that program copied once per key. Every scheduler so
far paid a cost proportional to the copies.

**Amendment 1. A (writer, seq) pair is not a sufficient timestamp.** A stateful
operator that has applied writer A's row and then joins it with writer B's row
emits a delta that depends on both writes. That delta needs a timestamp that
names both, which is a least upper bound in a lattice. Section 3.5 gives the
counterexample. Differential dataflow requires `Lattice` for this reason
(read). The alternative that needs no lattice is one global ticket order. It
is stronger than the ruling requires and costs no lock.

**Amendment 2. The binding graph is data, and evaluating it is recursion.**
rship users author a graph of 32,157 nodes and 35,000 wires. In the operator
model that graph is two collections, and "the value of node N" is a recursive
query over the wire collection. The wire graph is acyclic by validation
(read, `binding_node_connection.rs:120`), so the recursion can be stratified by
topological level. That evaluator is new work, and a relational operator is
the wrong shape for it. Section 5 argues it should be a compiled signal VM
hosted as one operator.

**Amendment 3. Demand does not come for free.** An operator over a collection
computes for every key. Per-key laziness has to be put back as data: the set
of demanded keys is a collection, and each family is restricted to it. This is
the magic-set idea and the domain of a dependent join (section 4.6).

**Amendment 4. The win is waiting, not arithmetic.** Today one thread holds
the settle path 98% of the time at 600,000 node fires per second (measured).
An operator engine does the same row work in batches across cores. I estimate
5 to 50 times less CPU per row, but that is an estimate. The certain gain is
structural: no per-write graph changes and no shape index to maintain.

**Amendment 5. There are two problems, not one.** myko is incremental view
maintenance over durable entities. The rship engine evaluates a user-authored
signal graph at 30 to 240 Hz. Per-datum instantiation hurts both, but the
remedies differ: collection operators for the first, a signal VM for the
second, and a split between a control plane and a data plane (section 5).

## 2. What the existing data says

### 2.1 Nodes per datum against nodes per program

Source: the partition census of the load-test server,
`.workspaces/partition-census/artifacts/census-run2/nodes-2265813.tsv`, one row
per live hyphae node with its creation site. The census ran on the
`hyphae-settle-core` branch, before `switch_map` was deleted, with the same
load configuration as the `h4` runs (32,157 background binding nodes).

| quantity | value |
| --- | --- |
| live hyphae nodes | 252,884 |
| distinct creation sites (file and line) | 127 |
| distinct (site, value type) pairs | 339 |
| nodes at sites that made 10 nodes or fewer | 56 (0.02%) |
| nodes at sites that made 1,000 nodes or fewer | 14,185 (5.6%) |
| nodes from the top 22 sites | 93.0% |
| nodes by owner of the site | hyphae 135,623, myko 60,815, rship 56,446 |
| sites by owner | rship 109, myko 13, hyphae 5 |

Read this as follows. At most 0.02% of nodes are "the program". The other
99.98% are a program site instantiated per key, a mean of 1,991 times per
site. The largest site, `pipeline/mod.rs:307`, is the materialisation point of
every per-key pipeline and accounts for 86,667 nodes.

To rerun:

```sh
cut -f4 nodes-2265813.tsv | sort | uniq -c | sort -rn | head -40
```

The brief said "about 32,000 nodes at setup". The run configuration gives
32,157 as the number of user-authored binding nodes, which are domain rows.
The hyphae graph that evaluates them is eight times larger.

### 2.2 Cone size and structure churn

`h4-w8-h4-e8-1.settle.3845381.clears` records one store write
(`store/registry.rs:63`) whose invalidation walk cleared 80,352 nodes
(measured, the "longest" column). The `h4` runs that produced this file did
not complete: `status.json` reports `deadline has elapsed`. I did not
re-derive the "half of all tickets change structure" figure. It comes from the
coordinator's brief.

### 2.3 The tick is 34 timer rows, not 25,000 writes

The load test's tick is not a client write of 25,000 values. It is 25 speed
nodes and 9 wave nodes sampled by a 30 Hz server timer (read,
`tools/reactive-bench/src/native/timers.rs`). Those 34 rows fan out through
speed, multiply, add, offset, switch and comp-element nodes to 3,600 cap
recipients. A cone-overlap analysis of an earlier run counted a median of
25,929 node fires in the runs a probe waited behind (measured,
`cone_overlap.out`). So "25,000 changes" is derived work. The input delta is
34 rows.

The probe is the only client write in the tick phase: one emitter pulsing at
10 Hz. Its cone is 4 to 8 nodes and shares no node with the tick (measured,
same file: 589 of 589 probe runs had zero overlap, and 51% of them waited a
median of 31.6 ms behind runs they did not intersect).

### 2.4 Baseline to beat

Verified runs only, median across samples, read from `result.json` under
`/mnt/tank/targets/hyphae-settle-core/load-runs/`. I did not run these.

| series | samples | probe RTT loaded p50 / p99 | probe RTT idle p50 | server cores | RSS | setup |
| --- | --- | --- | --- | --- | --- | --- |
| `m1t-w8-old-e8` (settle lock) | 2 | 52.2 / 101.1 ms | 0.34 ms | 4.33 | 9.1 GB | 64.9 s |
| `m1q-w8-new-e8` (pipelined) | 5 | 38.3 / 70.5 ms | 0.25 ms | 4.59 | 9.1 GB | 59.0 s |
| `m1q-w8-new-e16` | 2 | 2,745 / 5,133 ms | 0.26 ms | 3.59 | 11.5 GB | 85.4 s |
| `h4` and `h4reach*` | 0 verified | none | none | none | none | none |

From the settle statistics of the two e8 series: 600,000 to 632,000 fires per
second, 378 to 550 runs per second, and 0.97 to 0.98 seconds of settle time
per second (measured). One thread is saturated. That is about 20,000 fires per
tick at 1.6 µs each, and the whole 33 ms period is spent.

The limiter is known for these numbers. It is the single settle path. A probe
with a 4-node cone waits for a tick it does not touch.

## 3. The guarantees

### 3.1 The model

- A **writer** is a source of writes with its own sequence: a client
  connection, a timer, an effect. Writer `a`'s writes are `(a,1), (a,2), …`.
- A **write** is a finite set of input deltas, possibly across several input
  collections, applied atomically. myko needs this: one grouped mutation
  touches several entity types (read, `context.rs:1892`).
- A **collection** is a Z-set: a function from rows to integers with finite
  support. A delta is also a Z-set. Insert is weight +1, delete is −1, an
  update is a delete and an insert. Z-sets under addition form a commutative
  group. This is the DBSP data model [DBSP].
- A **program** `P` is a directed acyclic graph of operators from input
  collections to sinks. Recursion is an operator with an internal fixed point
  (section 4.4), not a cycle in the graph.
- A **cut** `C` is a set of writes that is closed under each writer's order:
  if `(a,n)` is in `C` then so is `(a,m)` for every `m < n`. `I(C)` is the sum
  of the input deltas of the writes in `C`.
- A sink **observes** a sequence of states. Each observation is tagged with
  the cut it claims to show.

### 3.2 Guarantee table

| # | guarantee | statement | rests on | by construction, or coordination and its scope |
| --- | --- | --- | --- | --- |
| G1 | Determinism | The observations of every sink are a function of the input writes and of the order in which writes are admitted (section 3.3). | Kahn 1974 for the function; Brock and Ackerman 1981 for why the admission order cannot be removed | Construction. Operators read FIFO channels and are sequential per shard. |
| G2 | Per-writer order | At every operator and sink, the effects of `(a,n)` precede the effects of `(a,n+1)`. | FIFO channels, and in-order release by timestamp | Coordination, per operator: the operator must learn that `(a,n)` has nothing more for it. This is progress tracking [Naiad]. |
| G3 | Causality | A sink never observes a cut that contains a write without the writes it depends on. A write made by a callback is admitted after the cut the callback observed. | Definition of cut, plus the callback rule (section 4.7) | Construction. |
| G4 | Glitch-freedom | Every observation of a sink `s` equals `P_s(I(C))` for the cut `C` it is tagged with, and a sink's cuts only grow. | Induction over the operator graph using the DBSP incremental identities (section 3.4) | Coordination, per operator: an operator with more than one input applies a write only when all of that write's deltas on all inputs have arrived. Scope is the operators the write reaches. |
| G5 | Convergence | When no writes are in flight, every sink shows `P_s(I(all writes))`. | Z-set deltas commute and sums telescope | Construction for Z-set operators. Order-sensitive operators (upsert at a store, `scan`) need arbitration per key, and G1 names the order. |
| G6 | Incrementality | The work to process a write is a function of the size of its deltas and of the state they touch, not of total state. Exact bounds per operator class in section 3.6. | DBSP theorems; differential dataflow | Construction, given indexed state. |
| G7 | Bounded waiting | A write's visibility at a sink waits only for writes that occupy an operator shard on its own path to that sink. | No theorem. This is a design property to measure (section 9). | Depends on the time model (section 3.5). |

Margara and Salvaneschi define five levels, each including the one before:
FIFO, causal, single-source glitch freedom, complete glitch freedom, and
atomic consistency [M&S]. Single-source glitch freedom means a read of a node
reflects all the effects of one update or none. Complete glitch freedom adds
that any two updates are seen in the same order at every node. Atomic adds
that one read across two nodes sees an update on both or on neither.

The ruling asks for their third level. G2 to G4 state it, with one
strengthening: every observation is the image of a whole cut. G4 does not
promise that two sinks observe the same sequence of cuts. Under a total
ticket order (section 3.5, model T1) every operator applies writes in the
same order, which is their fourth level. No model here gives the fifth,
because sinks advance independently. Their definitions are prose over what a
reader can observe. The statement of G4 as an equation is mine.

### 3.3 What ordering comes from structure alone

Kahn's model: sequential processes that communicate only over unbounded FIFO
channels, where a process is either computing or waiting on one named input.
Each process is a continuous function from input histories to output
histories, and the network's equations have a unique least solution that is
a continuous function of the inputs [Kahn]. Scheduling cannot change it. An
unfair scheduler can only produce a prefix. Kahn's paper does not prove that
the running network produces that solution. It defers the proof to Cadiou.
The result is standard, but the 1974 paper is not where it is proved.

This gives G1 for a single writer with no coordination. The only order it
gives is one total order per channel. Nothing orders two channels except the
order in which one process chooses to read them.

The model stops at merge. A fair merge must wait on two inputs at once, which
the model forbids, and no monotone function can be one: it would have to
return `a` for `(a, empty)` and `b` for `(empty, b)`, and then extend both
for `(a, b)`. Brock and Ackerman showed more. For networks with merge and
feedback, the relation between input and output histories is not
compositional: two components with the same relation can behave differently
inside a larger network [B&A]. Two independent writers that reach one
stateful key are a merge. So some order between them must be chosen, and no
structure can make that choice a function of the inputs.

The design consequence: choose the order once, at a named place, and make
everything downstream a Kahn network. The two honest places are ingress (one
ticket counter) or each stateful key (section 3.5).

### 3.4 Where coordination is unavoidable

The CALM theorem says a program has a consistent, coordination-free
distributed implementation exactly when it is monotonic [CALM]. The formal
version is Corollary 13 of Ameloot, Neven and Van den Bussche: a query is
computed by a coordination-free relational transducer network if and only if
it is monotone [Ameloot]. It is about the output set accumulated over a fair
infinite run, which is never retracted. It has no notion of intermediate
state, order of emission, or latency, so it proves nothing about glitches. Used as a
guide, it points at the right places:

- Linear operators (map, filter, flat-map, concat, negate) keep no state.
  Deltas from different writes pass through independently. No coordination.
- Join is bilinear. Its output delta for one write is
  `ΔA ⋈ B + A ⋈ ΔB + ΔA ⋈ ΔB`. It must see `ΔA` and `ΔB` of the same write
  together, or it emits a row that pairs new `A` with old `B`. That row is the
  glitch. The coordination is "all inputs of this write have arrived", scoped
  to that operator.
- Reduce is not monotone. Its output retracts earlier output. To emit the
  value of a group for a write, it must know the group's input for that write
  is complete. Same scope.
- A sink that performs an effect is the strongest case. An effect cannot be
  retracted, so the sink must know the state is complete for the cut before
  acting. This is the CALM observation in its plainest form: committing to an
  output that later input could contradict requires knowing the input is
  complete.

G4's proof is an induction. Assume each operator applies writes in an order
consistent across its inputs and applies each write with complete input. A
DBSP operator's incremental form satisfies
`out(prefix + w) − out(prefix) = Op^Δ(state(prefix), Δw)` [DBSP]. So the
accumulated output after a prefix equals the operator applied to the
accumulated input of that prefix. Compose along the graph. This argument is
mine. It uses published identities but is not itself a published theorem.

The completeness knowledge is what timely dataflow calls progress tracking.
Its safety is machine-checked twice: an abstraction of the Naiad protocol
with the TLA+ proof system [Abadi13], and a model of the Rust
implementation's protocol in Isabelle [Brun21]. Both prove safety only: a
reported frontier is never ahead of the truth. Neither proves liveness, and
neither verifies code. In one process, on a static acyclic operator graph,
the protocol reduces to a "write `t` is done here" punctuation on each
channel. That reduction is ours to check (gate L1-6, scenario S8).

### 3.5 Time models, and why (writer, seq) alone fails

**Counterexample.** `J1` and `J2` are stateful operators that both feed sink
`S`. Writer A's write `a` and writer B's write `b` reach both. Each operator
arbitrates locally. `J1` applies `a` then `b`. `J2` applies `b` then `a`.
`J2 = X ⋈ Y`, `b` inserts `x`, `a` inserts a matching `y`. `J2`'s delta for
`b` is empty and its delta for `a` is `(x,y)`. If `S` applies write `a` when
all deltas labelled `a` have arrived, it shows `(x,y)`, which contains `b`'s
row, without `b`'s contribution through `J1`. That is a glitch with respect
to `b`. The label `a` was wrong. The delta `(x,y)` belongs to "`a` and `b`".

Three time models repair this.

| model | timestamp | what a stateful operator does | proven by | cost |
| --- | --- | --- | --- | --- |
| T1 total order | one ticket from an atomic counter at admission | applies tickets in order | Kahn for determinism, DBSP for the deltas (Lean-checked theory) | A write waits at an operator shard for earlier tickets that have work there. |
| T2 lattice, all upper bounds | vector over writers | emits each output row at the least upper bound of its inputs' times. Reduce computes at every upper bound of concurrent times for a key | differential dataflow [DD]; a paper proof for locally finite orders and a small language [Abadi15] | No waiting. One input delta can produce output deltas at several later times [DD]. In the `differential-dataflow` crate, partial order rules out the `_total` operators, and its author measured the total-order `count` at 2.5 to 3 times the speed of the general one on his workloads. |
| T3 lattice, arrival order | vector over writers | applies writes per key in arrival order and stamps each output with the upper bound of everything applied to that key so far | the serialization-graph theorem of concurrency control (recalled, not re-verified this session), and a proof we would owe | No extra reduce work. A sink must apply all writes in a dependency cycle together, so two writes that truly share a key can wait for each other. |

T2 and T3 give "independent writers never wait". Both need a vector
timestamp and its compaction, and T3 needs a proof and a model check that do
not exist. T2 is the hypothesis as the coordinator stated it, and its
motivating example in the 2013 paper is exactly two independent sources with
times `(i, j)` under the product order [DD].

T1 gives less in principle and almost all of it in practice. Under T1 a
write waits only at an operator shard where an earlier ticket still has rows
to process. With a few hundred operators, each sharded by key, the probe of
section 2.3 shares no operator with the tick and waits for nothing
(inferred from its measured zero-overlap cone). A probe that does share an
operator waits for the tick's residence time in that shard, which section 9
bounds with a gate.

The earlier pipelined-ticket scheduler used the same order and failed. Its
cost was the footprint of a ticket over 250,000 nodes that changed shape on
half of the tickets. Over 300 static operators the footprint of a write is a
precomputed bitset per input collection.

**The specification is G1 to G7. The first implementation is T1.** T1
satisfies every guarantee, and the ruling permits it, since it rules that
global order is not required and does not forbid it. If the shared-path gate
of section 9 fails, move to T3 or T2 for the operators that fail it. The
operator algebra and the API do not change.

### 3.6 Incrementality, stated without overclaiming

From DBSP [DBSP], for a query `Q` on streams over an abelian group, with
integration `I` and differentiation `D`, the incremental version is
`Q^Δ = D ∘ Q ∘ I`, and:

- `(Q1 ∘ Q2)^Δ = Q1^Δ ∘ Q2^Δ` (chain rule, Proposition 3.2);
- if `Q` is linear and time-invariant, `Q^Δ = Q` (Theorem 3.3);
- if `×` is bilinear and time-invariant, then for delta streams `a` and `b`,
  `(a × b)^Δ = a × b + z⁻¹(I(a)) × b + a × z⁻¹(I(b))` (Theorem 3.4).

The paper states that all its theorems are checked in Lean. That covers the
theory and no implementation. DBSP's time is one synchronous step counter.
The identities are used here per ticket, which is that model.

What this buys per operator class:

| class | work per write | not promised |
| --- | --- | --- |
| linear | `O(|Δ|)` | nothing hidden |
| join, lookup, semi-join | `O(|Δ| · log n)` to probe, plus the number of matches produced | A delta row that matches a million rows produces a million rows. DBSP states the join bound two ways, and neither bounds the output delta. |
| reduce, linear aggregate (count, sum) | `O(|Δ|)` | needs an abelian aggregate |
| reduce, general (min, max, sort, top-k, collect) | the size of each touched group | DBSP says `MIN` is not linear and must track the full set to handle deletions. A one-row change to a 10,000-row group costs 10,000 unless the aggregate keeps its own ordered index. |
| levelled recursion | the number of derived rows that change, plus the scheduling cost per level | Diamonds are evaluated once per write only because of levels (section 4.4). |

"Cost proportional to the change" is true as stated only for linear
operators. For the rest the honest phrase is "proportional to the change in
input, the change in output, and the groups touched".

## 4. The capability set

### 4.1 Value types

| type | meaning | is |
| --- | --- | --- |
| `Coll<R>` | a multiset of rows that changes over time | a Z-set |
| `Keyed<K, V>` | a collection with at most one `V` per `K` | a Z-set with a functional dependency the engine checks |
| `Cell<T>` | one value that changes over time | `Keyed<(), T>` |
| `Events<R>` | rows that exist only in the write that carries them | a delta stream that is never integrated |

`Cell` stays. A UI with fifty cells is a program with fifty operators, each
holding one row, and it gets the same guarantees from the same engine. What
goes away is the server pattern of building a cell per key at run time.

State and events differ in one way. A collection's delta is added to its
accumulated value. An event's delta is delivered and forgotten. `hold`
integrates events into a collection. `changes` differentiates a collection
into events.

### 4.2 Operator algebra

| class | operators | notes |
| --- | --- | --- |
| linear | `map`, `filter`, `flat_map`, `concat`, `negate`, `map_with(udf)` | The UDF is a pure Rust function of a row. This is where per-type node logic goes. |
| bilinear | `join`, `left_join`, `semi_join`, `anti_join` | Both sides are indexed by the join key. An index is shared by every operator that reads the same collection by the same key [SA]. |
| lookup | `key.lookup(family)`, `lookup_each`, `lookup_set` | An equijoin whose right side is a family (section 4.6). The left side also supplies the demand. |
| grouped | `reduce`, `count`, `sum`, `min`, `max`, `top_k`, `collect`, `distinct` | `count` and `sum` are linear and run in `O(|Δ|)`. `min`, `max` and `top_k` keep an ordered index per group. |
| selection | `select(tag, [s1, …, sn])` | `tag` is a keyed collection of small integers. It compiles to a concat of `sᵢ` semi-joined with `tag = i`. All sources are fixed at build time. |
| event | `scan(state, f)`, `hold`, `changes`, `sample_on(events)` | `scan` is a keyed fold with explicit state. It is order-sensitive, so the order is the ticket order (T1). |
| recursion | `iterate` (general fixed point), `levelled` (stratified by a level column) | Section 4.4. |
| range | `range_lookup(lo, hi)` | For myko's `NumericFilter::Range`. An ordered index. |
| theta | `match_with(pred)` | Request rows against data rows of one type under an arbitrary predicate. For `Contains` and `StartsWith`. Cost is requests times delta, which is what myko pays today per scan instance (read, `registration.rs:916`). |

### 4.3 Time and timers

Time is an input. The engine never reads a clock inside an operator, because
an operator that does is not a function of its inputs and G1 fails.

- A clock is a writer. Each tick is a write of one row, `Tick(hz, n, t)`.
  rship already has one 240 Hz master clock and one shared source per rate
  (read, `libs/sync/src/clock.rs`, `value_runner.rs:228`).
- A timer is a row in a deadline collection. "Fire when due" is a range join
  of deadlines with the current tick.
- Sampling at a rate that is itself data is a join on `hz`. The audit's
  reactive-period sampler needs no special operator.
- Two rship maps read `Utc::now()` (`event_track_playback.rs:681`,
  `blend_reader.rs:411`). They become functions of the tick row's `t`.

There are no windowing operators. `throttle`, `debounce` and frame coalescing
are excluded (section 4.8).

### 4.4 Recursion

Two forms.

`iterate` computes a fixed point by repeating a body until no delta remains,
as in differential dataflow and DBSP's nested streams (the cycle rule of
Proposition 3.2). The published proof of differential's semantics covers
loops with a fixed iteration count, and its authors say the 2013 paper's
prioritised iteration appears incorrect outside the simplest time domain
[Abadi15]. So `iterate` here means the plain form only. Use it for transitive closure: target
lineage, myko's `GraphTraverseFrom`.

`levelled` is for evaluating an acyclic graph that is itself data. Section
5.3 develops it into a compiled signal VM. This section states its contract
as an operator. Inputs:
`Wires(src, dst)`, a per-node pure function, and `Level(node)`, the
topological depth. The operator processes one write's changed nodes in level
order, so each node is evaluated at most once per write with all its inputs
final. `Level` is maintained by a separate recursive operator that runs only
when wires change, which is user editing speed.

`iterate` on the same graph is correct but can evaluate a node once per
distinct path length. `levelled` exists to avoid that. The binding graph
qualifies because `validate_no_cycle` rejects cyclic wires at command time.
Reference edges (picker, `read_scene_output`, external node refs) are not
covered by that check (inferred from the rship read). They must either be
added to the level computation or evaluated by `iterate`. This is an open
item, not a solved one.

### 4.5 Effects and sinks

A sink is the only place an effect may happen. It receives, per write, the
complete delta of one collection, tagged with the cut.

- `sink(coll, f)`: `f(delta, cut)` runs once per write, in ticket order per
  sink.
- `keyed_effect(coll, on_insert, on_remove)`: the lifetime of a resource
  follows the presence of a row. This is the audit's P6: coordinators, peer
  connections, dispatchers.
- `snapshot(coll)`: a read from outside the graph. It returns the state at
  the sink's latest complete cut. It replaces every imperative `.get()` in
  commands and jobs.
- `await_visible(write, sink)`: resolves when a write is complete at a sink.
  This is the per-writer read-your-writes barrier. myko's command response
  already rides the client's FIFO behind its query updates (read,
  `ws_handler.rs:381`).

### 4.6 Demand and lifecycle

A family is a function from a collection of demanded keys to a keyed
collection of results, built once:

```rust
// today: runs once per key, builds nodes per key
let outputs = family(|k: &NodeKey| build_output_cell(k));

// proposed: runs once, builds operators
let outputs = Family::new(|keys: Keyed<NodeKey, ()>| -> Keyed<NodeKey, Value> {
    keys.join(&nodes).map_with(eval).into_keyed()
});
```

`key.lookup(&outputs)` does two things. It adds the keys in `key` to the
family's demand collection, and it joins `key` with the family's result. The
demand collection is a Z-set, so a key's weight is its reference count. When
the weight reaches zero the key leaves the demand, every operator in the
family retracts that key's rows, and index compaction frees them. Lease
counting, teardown order and "build in the ticket" are not separate
mechanisms. They are a delta of weight −1 flowing through joins.

This is the standard database construction. Neumann and Kemper replace a
dependent join `T1 ⟕dep T2` by a join of `T1` with `D ⟕dep T2`, where `D` is
the duplicate-free set of the outer bindings that `T2` refers to, and then
push `D` down until the right side has no free variables [N&K]. `D` is the
demand collection. Their 2015 paper claims this removes the dependent join
from any query and gives no proof. A 2024 note by Neumann supplies a paper
proof for a defined operator set. The same idea is the magic-set rewriting
of Datalog (recalled, not re-verified this session).

Noria is the closest published system that fills state on demand inside a
dataflow, with upqueries for missing keys. It guarantees eventual
consistency only, which is weaker than G4, so it is not a model to copy
[Noria].

A client subscription is a row `(client, sub, params)` in a request
collection. Results are routed by `(client, sub)` in one sink. Two clients
with the same params share all state up to the router. Today myko's query and
view subscriptions share nothing: each builds its own graph (read,
`ws_handler.rs:1557`, `:1646`), and with auth on, family scope is a fresh
session id so reports are not shared either (`context.rs:443`).

Installing new operators at run time stays legal. It is a program change: it
takes its own ticket and reads its inputs from existing indexes at that
ticket, as differential dataflow's `import` does [SA]. A UI mounting a
component does this. A server in steady state does not.

### 4.7 Writes from callbacks

A callback runs in a sink. A write it makes is a new write by the effect's
own writer. It is admitted after the cut the callback observed, so its ticket
is later. It is never part of the write that triggered it.

Reason: a write inside the triggering write makes the program cyclic, and a
cyclic network has a defined answer only as a fixed point, which an arbitrary
effect does not provide. A feedback loop through a sink is therefore a
sequence of writes. Whether it terminates is the application's property.

rship already behaves this way. Of 22 callback write sites, 17 are essential
and nearly all push the write out of the current settle with a spawn or a
scheduler. None needs its write to be visible in the same tick, and they are
idempotent by cursor or generation (read by a delegate, not re-verified by
me).

### 4.8 Deliberately excluded

| excluded | why |
| --- | --- |
| `switch_map`, `flatten` of a cell of cells, any operator whose output edges depend on data | The graph becomes data, and every guarantee that is cheap on a static graph becomes a per-write computation over graph shape. Already deleted. |
| Per-key closures that build nodes | Same reason. `Family::new` takes collections. |
| `.get()` inside an operator | A side read is an edge the engine cannot see. It breaks G4. Use `join` or `lookup`. |
| `throttle`, `debounce`, frame coalescing, any time window | Ruled out. They also break G5 unless time is an input, and then they are joins with a tick collection and can be written as such. |
| A global `settled()` barrier | Replaced by `await_visible(write, sink)`. |
| Wall-clock reads in operators | G1. |
| Last-write-wins coalescing of queued writes | It drops writes. `hold` on an event stream states the same intent and keeps G5. |

## 5. The layers, and the user graph as data

The stack is self-similar. rship's product is a reactive dataflow system:
users wire nodes together and clients get live values. hyphae is the same
idea for Rust code. Today the upper system is run by instantiating the lower
one once per element of the upper one.

### 5.1 What was checked

Verified by me in the `h4` worktrees:

- A binding node's input is computed by a lookup of the upstream node's
  output report. `input_resolution.rs:203-212` builds a
  `BindingNodeOutputValue { node_id, anchor_id, session_id }` from the wire
  and calls `.lookup(ctx.family::<BindingNodeOutputValue>())`. Each wire in
  the user's graph is therefore a chain of hyphae cells, and a user chain of
  depth `d` is a hyphae chain of depth proportional to `d`.
- `Pulse` is a myko store entity (`#[myko_item]`, `pulse.rs:241`). Its own doc
  comment says "a pulse is an event". The server registers a
  `BlackholePersister` for it, and for `ExternalNodeOutputValue`,
  `TargetStatus`, `EmitterStatus`, `ActionStatus`, `TargetLayout` and
  `LinkLog` (`apps/server/src/runtime.rs:72-90`). So the application already
  declares these types non-durable, and they still take the store, index,
  query and family path.
- `Pulse` has no timestamp field.

Measured by me from the census of section 2.1, by value type: 18,381 nodes
hold an `InputAnchorResolution` and 10,711 hold an `Option<BindingValue>`.
Those two types alone are 11.5% of the graph. About twenty other value types
appear 3,600 times each, once per cap recipient: instance and target lists,
layouts, statuses, assigned-instance outputs, reconciled properties. They are
routing and required-state views, not signal evaluation, and together they
are a larger share of the node count than the binding types.

Reported by the coordinator and not re-derived by me: fired nodes at heights
up to 204, about half of a tick's fires above height 100, and a repeating
unit of about 13 hyphae nodes per stage of a cascade. A delegate's static
count is consistent with the unit: about 40 cells per input anchor and 17 to
20 per output anchor, before sharing. The mapping of 13 nodes to one wire is
an inference.

What this supports. The nesting explains the depth and the tick's cost: a
height of 204 at 13 per stage is a user chain of about 16 nodes. It does not
explain all of the node count. Most nodes are per-key control-plane views.
Both are per-datum instantiation, and they need different remedies.

### 5.2 What each layer is for

**myko is incremental view maintenance.** It holds the durable source of
truth and keeps client-visible views of it current. Its inputs change at
human and device-status rates. Its hard problems are joins, sharing between
subscribers, demand, atomic multi-entity writes, and consistency of what a
client sees. Sections 3 and 4 are a specification for this problem.

**The rship engine is a signal-graph virtual machine.** It evaluates a
user-authored graph at 30 to 240 Hz. Structure changes at editing speed and
values change on every tick. This is the problem an audio graph, a node
compositor and a game-engine update loop solve, and the usual solution is the
same in all of them: compile the graph to a topologically ordered schedule
over dense value slots, and recompile the affected part when the user edits.
I state that from general knowledge. No source was checked for it this
session.

The two problems want different data models. View maintenance needs
multisets with retraction, because rows join, group and disappear. A signal
is a register. Its change is an overwrite of one slot, and representing it as
a retraction and an insertion keyed by `(node, anchor, session)` through hash
indexes pays for generality the signal never uses.

**What remains for hyphae.** Three things:

1. The operator engine for view maintenance, which myko is built on.
2. The contract that both engines obey: writers, tickets, cuts, sinks, the
   callback rule, and the guarantees of section 3.2. This is what makes a
   client's view of configuration and of live values consistent with each
   other.
3. Cells for client code. A UI's own program is small, and per-datum cells
   are the right tool there.

hyphae should not be the evaluator of the user's graph. It should be the host
that the evaluator plugs into.

### 5.3 Should the lower engine evaluate the upper graph as data

Yes. The alternative, instantiating the lower system per element, is the
status quo and is what the census measures. The question is which evaluator.

| | (i) one engine, user graph as collections, `iterate` | (ii) a dedicated signal VM, hosted as one operator | (iii) status quo |
| --- | --- | --- | --- |
| representation of a node output | a row `(node, anchor, session, value)` in a Z-set | a slot in a dense array per session | 17 to 40 cells |
| evaluation of one tick | repeated join of changed outputs with the wire index until no delta remains | one pass over the dirty part of a precompiled schedule | a height-ordered walk of up to 204 levels of cells |
| a diamond in the user graph | may be evaluated once per path length unless levels are added | once, by schedule order | once, by height |
| glitch-freedom inside the graph | at the fixed point only. Intermediate rounds are hidden by the operator boundary. | by construction: a node runs after its inputs. This is the same argument as SID-UP's proof on a fixed graph. | by the settle scheduler |
| a user edit | a delta on the wire collection | recompile the schedule for the affected scene | rebuild cells, which is the structure churn |
| cost per changed value | hash probe, retraction and insertion, per round. Estimate 0.2 to 1 µs. | array read, a function call, array write. Estimate 20 to 100 ns. | 1.6 µs measured |
| per-type node logic | a UDF | the same UDF | `make_cell` |
| stateful and time nodes | `scan`, join with ticks | a state slot and the tick value as an input slot | cells |
| data-dependent references (picker, comp-element ref) | a join, natural | needs an over-approximated edge or an indirect read. The awkward case. | lookup |
| who owns the code | hyphae | rship, behind a hyphae operator trait | both |
| theory to lean on | DBSP cycle rule, differential iteration | topological evaluation of an acyclic graph | none that covers it |

(i) is correct and general. It is the wrong shape for the tick, by a factor
I estimate at 5 to 10 and have not measured. (iii) is what failed.

**Choose (ii), hosted as an operator.** "Hosted" matters. The VM is not a
separate system beside hyphae. It is one operator in the hyphae graph:

- Its control inputs are collections: nodes, wires, node values, active
  scenes, sessions, assignments. A change to them arrives as a ticket with a
  complete delta, and the VM recompiles the affected schedule at that ticket.
  G4 holds across the boundary, so the VM never sees half of an edit.
- Its signal inputs are events: pulses and clock ticks.
- Its outputs are events for sinks (executor sends) and keyed values for
  views that clients watch.

Section 4.4 called this operator `levelled`. The framing here is the better
one: it is a compiled VM, not a relational operator, and the general
`iterate` is kept for transitive closure over control-plane data only.

The 128 node types divide the same way as in section 7.2. The 76 pure types
and the 15 with lookups are VM instructions, with lookups resolved to slots
at compile time. The 12 stateful and 6 time types get a state slot or read
the tick slot. The 10 effect types write to an output queue that a sink
drains. The 9 data-dependent types are the open design problem.

### 5.4 Control plane and data plane

Splitting them is the right design, and the code already half-admits it by
sending seven entity types to a persister that discards them.

| | control plane | data plane |
| --- | --- | --- |
| content | configuration and entities: nodes, wires, scenes, targets, assignments, sessions, subscriptions | signals and pulses: emitter values, node outputs, clock ticks, action calls |
| rate | editing speed, device status changes | 30 to 240 Hz per signal |
| durable | yes | no |
| algebra | multisets with retraction; join, group, demand | registers and events; overwrite and append |
| engine | hyphae operators (myko) | the signal VM |
| unit of work | a write, atomic across entity types | a pass over one scene's dirty schedule |
| must guarantee | G1 to G6; a client never sees a partial write; durability before acknowledgement where myko promises it | per-source order; every pulse evaluated, none coalesced; each pass reads one consistent set of inputs; bounded latency; never waits for the control plane |
| may relax | latency, to milliseconds | durability, history, cross-source order |

How the planes meet:

- **Control to data.** A control ticket produces a new compiled schedule and
  slot layout for the scenes it touches. The VM switches to it between
  passes. A pass never observes two versions. Recompilation cost is paid by
  the control ticket, not by the tick.
- **Data to control.** A live value a client watches leaves the VM as a keyed
  output and is routed by the subscription table, which is control-plane
  data. It does not enter the store. A value that must become durable is
  written by a sink as an ordinary write, under the callback rule of
  section 4.7.
- **One clock of tickets.** Passes and control writes share the ticket order
  where they touch the same scene, so "the edit happened before or after
  this pass" always has an answer.

A pass is triggered by an input, not by a timer of the engine's own. A clock
tick is an input. A pulse is an input and gets its own pass over its own
cone. This keeps the ruling against frame coalescing: the VM never holds a
pulse to batch it with the next tick.

**Why the data plane can do without cross-writer order.** "Latest wins by
timestamp" is a maximum, and a maximum over a total order is commutative,
associative and idempotent. Merging two sources by it is monotone, and CALM's
direction applies: a monotone merge needs no coordination and every
interleaving converges. That is the precise content of the ruling that PTP
alignment makes global order unnecessary. It holds for registers. It does not
hold for pulses that act as triggers, where each one must be seen once and in
its source's order. Those need per-source FIFO, which G2 gives.

**The gap.** The argument needs the timestamp to be in the data. `Pulse` has
none. Today "latest" in the server means "arrived last", which is an order
chosen by the network and the store, not by PTP. If alignment happens in
executors and SDK streams, the server's merge is still arrival order. Either
the pulse carries its timestamp into the VM, or the claim should be restated
as "arrival order per source, and the application tolerates the rest".

### 5.5 What this changes in the plan

The falsifying experiment of section 8.1 is unchanged in content. It is the
tick path, and it should be written as the VM.

The order of work changes. The VM can ship first, against today's hyphae:
it reads control data through ordinary subscriptions and replaces the
binding value reports, input resolution and the per-type `make_cell`. That
removes the deep cascade and the tick's fires from the old engine before the
operator engine exists. The per-recipient routing views and myko's
per-subscription queries stay per-datum until the operator engine replaces
them. Whether the old engine is then fast enough for what is left is
measurable after that step and unknown before it.

## 6. Execution models

### 6.1 The three options

**A. Per-datum nodes with source sets.** Keep one node per datum. Replace
heights and certificates with SID-UP's rule: each node stores the set of
sources it is reachable from, each turn carries the set of changed sources,
and a node waits for every dependency whose set intersects the turn's
[SIDUP]. DREAM's glitch-free protocol buffers at a node in a similar way
using per-source timestamps [M&S].

What the papers themselves say about this option:

- SID-UP turns "must happen mutually exclusive", through a central
  coordinator. Concurrent turns are listed as ongoing work. A topology
  change invalidates the source sets of the changed node and everything
  downstream, and the paper gives no cost bound for it [SIDUP].
- DREAM "assumes that the dependency graph is fixed", and its authors call
  combining concurrent propagation with glitch freedom "an open research
  problem" [M&S].
- MV-RP is the published scheduler for concurrent turns on a per-datum graph
  with dynamic edges. It proves abort-free strict serializability using
  two-phase locking in a framing phase plus multiversion reads, and reports
  a 25% single-thread slowdown against a global lock [TSRP]. It is the
  locks-and-versions design the rulings exclude.

**B. An operator graph over collections, built in-house.** Section 4, with
Z-set batches, shared indexes, sharded operators, and T1 tickets.

**C. A front end over an existing engine.** `timely` with
`differential-dataflow`, or `dbsp`.

### 6.2 Facts about the crates

Checked on 2026-11-20 by a delegate that read the published sources. I did
not re-read them.

| | timely 0.32 + differential-dataflow 0.26 | dbsp 0.385 |
| --- | --- | --- |
| licence | MIT | MIT OR Apache-2.0 on the crate. The repository has an enterprise carve-out. |
| maintenance | active. One person wrote 57 of 81 and 97 of 100 commits in the last year. A breaking minor about every six weeks. Differential is mid-rewrite. | active, released almost daily, versioned with the vendor's platform, no semver stability |
| dependencies | light | heavy and not optional: a storage layer, `rkyv`, multi-thread `tokio`, `nix`, compression. Every key and value type must implement `rkyv` archiving. |
| threads | one thread per worker, each with a copy of the dataflow. A worker can be stepped from a host thread (`examples/threadless.rs`). Inputs are not `Send`; other threads feed a channel. | spawns its own workers. The host cannot own them. |
| new operators at run time | yes: `worker.dataflow`, `drop_dataflow`, `import` of a shared index | no. The circuit is fixed. The only change path rebuilds a second circuit. |
| time | partial order in timely, `Lattice` in differential. Product times make two writers incomparable. | one global step clock, nested for recursion |
| scheduling | cooperative per worker. Join yields. Reduce "runs the whole interval". | one global step for all workers |
| wasm | timely's CI checks `wasm32-unknown-unknown` single-threaded. Differential has no wasm CI; an open issue reports it running in a browser after `Instant` patches. | no evidence, and the dependency set makes it implausible (not compiled) |
| published latency | 0.64 ms lookup and 1.28 ms two-hop at batch size 1 on a 10M-node graph; p99 of 6 ms at 1M updates/s on 8 workers [SA] | throughput benchmarks only |
| fixed cost | progress tracking measured at 333 ns per operator per step for 200 operators (open PR 838) | not found |
| formally proven | progress-tracking safety in Isabelle [Brun21]; differential semantics on paper [Abadi15] | the theory in Lean [DBSP]. Not the Rust code. |

Materialize's own crates are under BSL 1.1 and cannot be a dependency.

### 6.3 Comparison on rship's two workload shapes

Workload 1 is the 30 Hz tick of about 25,000 derived changes. Workload 2 is
a small probe write that must not wait behind it.

| | A per-datum + source sets | B in-house operators | C1 timely + differential | C2 dbsp |
| --- | --- | --- | --- | --- |
| tick throughput | 25,000 node fires, each a pointer chase and a lock. Today 1.6 µs per fire on one thread (measured). Source-set checks add work per fire. | 25,000 row-operations in perhaps 10 operator batches. Estimate 0.1 to 0.5 µs per row, so 2.5 to 12 ms on one thread, and it splits by key across cores. | Same batches. Generic sorted-batch indexes cost more per row than dense arrays would. Published throughput is millions of updates per second per core on simple operators. | Same batches, plus `rkyv` bounds on every value. |
| structure churn | Unchanged. Half of tickets change edges, and SID-UP must re-propagate source sets downstream of each change. This is the wall the reach index hit. | None. A key change is a delta through a join. | None. | None. |
| probe latency | Bounded by source-set bookkeeping over a 250,000-node graph. No argument that it beats the reach index. | T1: waits only at shards it shares with the tick. Zero wait for the measured probe (inferred). Worst case is the tick's residence in one shard. | Total order: the probe's output waits for the frontier to pass the tick. Product times: no frontier wait, but the probe's rows still wait for the running operator activation on its worker, and reduce does not yield. | Waits for the step in flight, always. |
| all cores | Parallel waves exist today and reach 4.3 cores for the same throughput as one. | Data-parallel inside an operator, pipelined across operators. | One thread per worker, data exchanged by key hash. | Yes. |
| layout for hardware | Scattered heap nodes. No path to SIMD. | Ours to choose: columnar batches, dense ids. | Columnar batches are arriving in 0.26. | Columnar, file-backed. |
| client queries at run time | New roots per subscription. | Rows in a request collection. | Rows, or `import`. Both work. | Rows only. |
| wasm clients | Works today. | Single-threaded executor of the same operators. | Possible for timely. Unverified for differential. | No. |
| already proven | Glitch freedom of SID-UP for one turn at a time on a fixed graph [SIDUP]. | Kahn determinism, DBSP incremental identities, progress-tracking safety, all for the design, none for our code. | The same, plus the protocol proof matches this implementation's design. | DBSP theory. |
| we must prove or test | Concurrent turns with dynamic edges. The model in `docs/settle-epochs-model` is an attempt at this. | That our ticket punctuation is a correct special case of progress tracking. That each operator equals its from-scratch definition (testable against an oracle). `levelled`. | The front end's translation. Our own `levelled` operator inside their framework. | The front end. |

### 6.4 Reading the comparison

A keeps the property that caused the problem. Source sets remove the need for
heights, but a source set is still a fact about graph shape that must be
maintained when shape changes, and shape changes on half the tickets. The
literature offers no concurrent, glitch-free, lock-free algorithm for a
dynamic per-datum graph, and its authors say so. Three schedulers of local
evidence agree.

C2 fails on verified facts: no run-time changes to the circuit, no wasm, a
global step, and a dependency set that makes it a platform and not a library.

C1 is the only existing engine that fits. It is weak exactly where the
requirement is strict. Its scheduling unit is an operator activation on a
single-threaded worker, so a probe waits behind a tick's reduce on the same
worker. Partial-order times remove the frontier wait and cost the `_total`
operators plus unquantified reduce overhead. It has one maintainer and a
breaking release every six weeks during a rewrite. And `levelled`, the
operator the tick spends its time in, would be ours anyway.

B takes the proven designs and none of the code. It owns the two things the
requirement is about: how a shard is scheduled, and how rows are laid out.

## 7. Fit with real code

### 7.1 myko

Counts are from a delegate's read of `myko/libs/myko` on `h4`. There are 51
query, view and report definitions after removing tests and wrappers: 27 from
macros, 24 hand-written.

| class | count | becomes |
| --- | --- | --- |
| linear | 4 | `map`, `filter` |
| join, semi-join, lookup by key | 24 | `join`, `lookup` against the request collection |
| grouped | 6 | `count`, `reduce` |
| recursive (graph traversal, today a full re-traversal per edge diff) | 2 | `iterate` |
| opaque | 7 | see below |
| constant stubs | 8 | constants |

34 of the 43 non-stub definitions (79%) are operators with params as a key
column. With `iterate` it is 36 (84%).

The filter language is already data. `XQuery` is a struct of optional
per-field filters with `Eq`, `In`, `Range`, `Contains` and `StartsWith`
(`query/filter.rs:168`). `Eq` and `In` on an id or foreign key are equijoins
with the request collection. `Range` is a range lookup. `Contains` and
`StartsWith` are residual predicates. The closure escape hatch `test_entity`
has 5 production uses, all trivial.

The 7 opaque definitions are search, tree export, two permission
inspections, persist health, and entity history. Each reads something outside
the graph: a search index, a database page, atomics on a 500 ms poll. They
become input collections fed by the owner of that state, or they stay as
explicit request-and-response functions that are not reactive.

`ReportHandler::compute(&self, ctx)` does not survive as written. `self` is a
concrete value consumed at build time, and the return is one cell per
instance. The surviving shape takes the params as a column:

```rust
fn build(req: Keyed<ReqId, Self>, ctx: &Ctx) -> Keyed<ReqId, Arc<Output>>;
```

Of 31 report bodies sampled, 4 branch on params in Rust and 9 use snapshot
reads. No user report holds mutable state per instance. Request identity
(`client_id`, `host_id`, auth) is read inside bodies today and must become
key columns. It must not partition state per session, which it does today.

myko's ordering needs match the guarantees: per-writer order, an atomic
batch across entity types, and a barrier for "my write reached my
subscriptions". Nothing in the live path uses a cross-writer total order
(inferred by the delegate). The Postgres log has one, for replay.

### 7.2 rship

From a delegate's read of rship `h4`. Production code has 0 `switch_map`
calls, 105 `.lookup(`, 38 `lookup_set(`, 24 `lookup_each(`, and 72 distinct
family types.

Node types, 128 in production:

| class | count | share | becomes |
| --- | --- | --- | --- |
| pure function of inputs and config | 76 | 59% | `map_with(udf)`, the body unchanged |
| pure with a lookup | 15 | 12% | `join` then `map_with` |
| stateful per (node, session): edge detect, latch, on-change | 12 | 9% | `scan` with explicit state |
| time-dependent: interval, ramps, fades, samplers | 6 | 5% | join with `Tick` |
| effect: sends to executors, commands | 10 | 8% | sink |
| data-dependent structure: picker, comp-element ref | 9 | 7% | join on a data-dependent key, or reduce with arity from data |

91 of 128 (71%) are map, join or reduce with `(node, anchor, session)` as key
columns. Of 12 audit sites re-read on `h4`, 10 are pure per-row. The other two
read the wall clock or pause a runner from a subscribe. The delegate found no
site that needs an irreducible per-key subgraph. That is an inference from
sampling, not an exhaustive result.

The hot path as operators, from the delegate's trace:

```
Src         = Pulses(e, client, v) ⋈ Attached(e, s, client, scaling)       join, map
EmVal(e,s)  = max_by_pulse(Src)                                            reduce
EmExec      = scan(Src by (e, s, path))                                    stateful
Exec(n,s)   = Nodes ⋈ Sessions ▷ Blocked ▷ Disarmed                        anti-joins
In(n,a,s)   = coalesce(Wires(n,a ← m,b) ⋈ Out(m,b,s), Fields, Literal, Default) ⋈ Schema
Row(n,s)    = reduce(In by (n, s))                                         reduce
Out(n,a,s)  = Out0 ∪ map_with(Row ⋈ Exec, udf[type])                       recursive through Wires
Calls       = scan(In ⋈ ActionDef by (n, s)) ⋈ Routing → sink(send)        stateful, sink
```

`In` and `Out` are mutually recursive through `Wires`. That is the
`levelled` operator. `Blocked` is a cycle guard over wires and is a graph
algorithm of its own.

Today one emitter-to-action binding with one input costs about 120 to 140
cells per session (the delegate's static count, not deduplicated). The wide
fan-out comes from whole-collection reads inside per-key cells:
`GetActiveScenesByQuery{session}.items()` is read by every
`(node, anchor, session)` output cell (`value_reports.rs:85`), so one
`ActiveScene` write reaches all of them. In the operator model that read is
one join.

Two findings that bear on the rulings:

- `subscribe_exec_events_with_retry` (`exec_line.rs:43`) replays the last
  exec event when the active scene changes, because two paths from one write
  arrive at different times. That is a hand-written repair for a glitch. G4
  removes the need.
- The ruling says rship aligns payloads by PTP timestamp at the application
  level. The delegate could not find that in the server. `Pulse` and
  `ExecTargetAction` carry no timestamp, and PTP appears only in NMOS and
  telemetry metadata. If alignment lives in executors or SDK streams, it was
  not read. **Confirm this before relying on it**, because it is the reason
  given for not needing a cross-writer order. Under T1 the question is moot.

### 7.3 What opaque per-key closures become

| today | count (approx.) | becomes |
| --- | --- | --- |
| closure that maps a key to another report's cell | ~105 `lookup` sites | `key.lookup(&family)`, unchanged at the call site |
| family body that builds cells for one key | 72 family types | a body over `Keyed<K, ()>`, written once |
| per-node-type `make_cell` | 89 `cell` node types | 15 to 20 need joins. The rest are a `map_with` UDF or a `scan`. |
| `project_cell` per row then `join_vec` | ~45 | `join` then `reduce` |
| subscribe that creates per-row resources | ~8 | `keyed_effect` |
| query built inside a timer or command callback | ~12 | `snapshot`, or hoisted |

## 8. Recommendation

**Build B, in two parts. First a signal VM for the rship engine, hosted as
one operator. Then an in-house operator engine over Z-set collections for
myko, with T1 tickets and demand as a collection.** Use differential dataflow
as the semantic reference and as a test oracle, not as a dependency.

Reasons, in order of weight:

1. The measured graph is 127 program sites copied 2,000 times. Every
   scheduler's cost was the copies.
2. 79 to 84% of myko's definitions and 71% of rship's node types already have
   the operator shape, and no site was found that needs a per-key subgraph.
3. The two things the requirement is strict about, shard scheduling and
   layout, are the two things an adopted engine would not let us own.
4. T1 over a static operator graph needs no new theory. Each piece is a
   published result.
5. The tick's cost is the user's graph run as nested cells (section 5.1).
   The VM removes that without waiting for the rest.

### 8.1 The cheapest falsifying experiment

Build the load test's own tick path as operators, standalone, with no myko
and no rship crates. It is the path the tick spends its time in, and the data
to compare against already exists.

- Tables generated from `reference-scale.json`: 32,919 nodes with the real
  type histogram, 35,984 wires, 181,842 values, 25 active scenes, 3,600 cap
  recipients.
- Program: `Tick(30)` ⋈ 34 sync values → speed and wave UDFs → the signal VM
  over wires with the calc, switch and offset UDFs → join to recipients →
  counting sink. About 12 operators, one of which is the VM.
- Second writer: a probe row at 10 Hz through its own 3 operators, and a
  variant that enters the VM in a scene the tick also drives.
- A third writer edits one wire per second, to exercise recompilation.
- Values are a small enum like `BindingValue`. No serialization.

It falsifies the recommendation if any of these holds:

1. The tick takes more than 3.3 ms at p99 on 4 threads (gate L1-1). Then
   batch operators over this data are not fast enough to leave room for
   myko's costs, and the layout argument is wrong.
2. The disjoint probe exceeds 0.25 ms at p99 while the tick runs (L1-2).
   Then T1 does not isolate independent writers and the time model must
   change before anything else.
3. The path cannot be written without building an operator per node, per
   wire, or per session. Then the hypothesis fails on expressiveness.

Effort: three to five days for one engineer who knows the codebase, since it
needs only vectors, hash maps and a thread pool. The level-1 suite in
`.workspaces/h4-exp` is this experiment plus the stress tests of section 9.

### 8.2 Effort

These are estimates with wide error. Each assumes one engineer with agent
help.

| stage | content | estimate |
| --- | --- | --- |
| 0 | the experiment above | 3 to 5 days |
| 1 | signal VM in rship against today's hyphae: schedule compiler, slots per session, 128 node types as instructions (76 keep their body), state and tick slots, output queue, recompile on edit. Replaces the binding value reports and input resolution. | 5 to 8 weeks |
| 2 | hyphae operator engine: batches, shared indexes, the operator set of section 4.2, tickets and punctuation, sinks, demand, `iterate`, the operator trait the VM plugs into, single-thread wasm executor, oracle tests | 5 to 8 weeks |
| 3 | myko: stores as input collections, the filter language as joins, the request collection and router, report and view traits, macros | 4 to 8 weeks |
| 4 | rship control plane: the remaining families and about 220 sites, the routing and required-state views, the sinks | 6 to 10 weeks |

Five to eight months in total. Stage 1 delivers the tick improvement alone
and is the point to re-measure. Stages 1 and 2 are independent and can run
in parallel. Stage 4 is the most mechanical.

### 8.3 Risks

1. **The VM and reference edges.** Wires are acyclic by validation.
   Picker, scene-output and external references are not checked. If they
   form cycles in real projects, the tick path needs `iterate`, which
   evaluates more. Check on a real project file early.
2. **Value cost.** Level 1 uses small values. Real rows are `Arc<dyn AnyItem>`
   and 512-byte structured payloads. If a row operation costs 2 µs because of
   clones and dynamic dispatch, batching gains little. Level 2 exists to
   catch this.
3. **Shared-shard waiting under T1.** A probe that enters a shard the tick
   occupies waits for it. If L1-3 fails, the fix is T3 or T2 on that
   operator, which is new protocol work with a proof owed.
4. **Opaque reports.** Seven myko definitions and the unread parts of rship
   may hide state the classification missed. The rship count is a sample.
5. **Demand cost.** A family restricted to its demand pays a semi-join on
   every input. For a family that nearly everything demands, that is pure
   overhead, and the family should declare itself total.
6. **Rewriting twice.** rship just finished moving 220 sites from
   `switch_map` to `lookup`. Call sites survive. Family bodies do not.
7. **The estimate of 0.1 to 0.5 µs per row.** I did not measure it. The
   experiment does.
8. **Data-dependent node types.** Nine node types choose what they read from
   a value. A static schedule must over-approximate their edges or read
   indirectly. Neither is designed.
9. **The VM boundary against today's hyphae.** Stage 1 feeds the VM from
   subscriptions on the old engine, which calls back once per collection
   and not once per write. An edit that spans collections may reach the VM
   in pieces until stage 2, and the VM must tolerate recompiling twice.
10. **Timestamps.** The data plane's freedom from cross-writer order rests on
    a timestamp that `Pulse` does not carry (section 5.4).

### 8.4 Migration shape

`key.lookup(family)` survives as surface syntax, with the same meaning to
the caller. `lookup_each` and `lookup_set` survive as joins. What changes is
on the other side of the call:

| surface | today | after |
| --- | --- | --- |
| `key.lookup(&fam)` | takes a lease on a per-key row | equijoin plus a row in the demand collection |
| `family(|k| …)` | closure per key returning a cell or map | `Family::new(|keys| …)` over collections, run once |
| `Cell<T>` | a node | `Keyed<(), T>`, same methods |
| `CellMap<K, V>` | a node with diffs | `Keyed<K, V>` |
| `cell.map(f)`, `a.join(b)` | per-datum operators | the same names on collections |
| `ReportHandler::compute(&self, ctx)` | one cell per params | `build(req: Keyed<ReqId, Self>, ctx)` |
| `ctx.query_map(q)` | builds or leases a view | `requests.lookup(&query_family)` |
| node `make_cell` | builds cells per node | a UDF registered per node type, or a `scan` |
| `subscribe` | callback on a node | `sink` |
| `.get()` | read under the settle | `snapshot` outside operators, forbidden inside |
| `hyphae::submit`, tickets, epochs, `SettleKey` | public | gone. A write is `engine.write(|w| …)`. |

Order of work: the signal VM, then the operator engine, then myko's stores
and generated queries (the 27 macro-generated definitions move at once),
then rship families one entity crate at a time behind the unchanged `lookup`
call sites, then delete the per-datum engine. A family written the new way can be
read by `lookup` from old code through an adapter that sinks its output into
a `CellMap`, so the two engines can coexist during stage 3. The adapter costs
one ticket of latency at the boundary and should not survive the migration.

## 9. Validation ladder

Three levels. Do not start a level until the one below passes every gate.
Each gate is derived from a guarantee in section 3.2 or from the targets:
probe round trip well under 7.7 ms while a 30 Hz tick of 25,000 changes
runs, and nothing dropped.

### 9.1 How the numbers are derived

- The tick period is 33.3 ms. "Nothing dropped" with no time windows means
  the queue must not grow, so tick service time must stay below the period
  with headroom for bursts and for the e16 scale. The budget is 10% of the
  period at level 1, 30% at level 2, and 50% at level 3. Today it is 98%.
- The probe target is an end-to-end round trip. The idle round trip today is
  0.34 ms at p50 and 0.66 ms at p99 (measured, section 2.4), and that is the
  transport floor. "Well under 7.7 ms" is taken as p99 at or below one third
  of it (2.5 ms) and the maximum below it. That leaves about 1.8 ms at p99
  for everything the server adds under load. Level 1 may spend 0.25 ms of it
  and level 2 may spend 1.0 ms.
- G7 says an independent write does not wait. The test of that is a ratio:
  loaded latency against idle latency for the same probe.
- Correctness gates are zero-tolerance counts.

Every latency gate is reported with run count, spread and the limiter, on a
machine with other load below one core. A gate passes only if it passes in 5
of 5 runs.

### 9.2 Level 1: hyphae alone

In-process, no myko, no serialization, small enum values. Release build, 4
worker threads unless stated. This is the specification for the suite in
`.workspaces/h4-exp`.

Scenarios:

- **S1 tick.** The program of section 8.1 at reference scale. One writer, 34
  input rows per tick at 30 Hz, about 25,000 derived row changes, 3,600 sink
  rows. 60 seconds after a 5 second warm-up.
- **S2 disjoint probe.** S1 plus a second writer at 10 Hz whose rows pass
  through operators the tick never reaches. Also run idle, with no tick.
- **S3 shared probe.** S1 plus a probe that enters the VM in a scene the tick
  drives, on a node the tick does not touch.
- **S4 key churn.** S1 where half the writes also change a lookup key, and a
  request collection gains and loses 1,000 rows per second.
- **S5 scale.** S1 with state scaled to 10 times the rows and the same 34
  input rows and the same derived change count.
- **S6 cores.** S1 on 1, 2, 4 and 8 threads.
- **S7 oracle stress.** 8 writers, random programs drawn from the operator
  set, random inserts, deletes and key changes, at least 10⁷ writes in
  total. Every sink records each observation with its cut. A from-scratch
  evaluator recomputes `P(I(C))` for each recorded cut.
- **S8 schedule exploration.** The ticket and punctuation protocol on a
  3-operator diamond with 2 writers and 3 writes each, under a model checker
  or an exhaustive schedule explorer.
- **S9 demand.** A family with 10,000 possible keys, demand cycling between
  0, 100 and 10,000 keys.
- **S10 wasm.** S1 and a reduced S7 on `wasm32-unknown-unknown`, one thread.
- **S11 edit under load.** S1 while a writer adds or removes one wire per
  second in a scene the tick drives.
- **S12 control burst.** S1 while a control writer inserts 10,000 rows into a
  collection the VM does not read, and then 1,000 rows into one it does.
- **S13 pulse train.** One emitter pulsing at 1 kHz with distinct values
  through a 5-node chain to a counting sink, during S1.

Gates:

| gate | from | scenario | quantity | pass |
| --- | --- | --- | --- | --- |
| L1-1 | stability | S1 | tick completion, admission to last sink row, p99 | ≤ 3.3 ms |
| L1-2 | G7, probe target | S2 | probe admission to sink, p99, loaded | ≤ 0.25 ms |
| L1-2b | G7 | S2 | loaded p99 divided by idle p99 | ≤ 2.0 |
| L1-3 | G7, probe target | S3 | probe p99, loaded | ≤ 1.0 ms. A fail with L1-2 passing means "change the time model inside the VM", not "abandon". |
| L1-4 | nothing dropped | S1 to S4 | queue depth at end, and maximum queue depth | 0 at end, never above 2 ticks |
| L1-5 | nothing dropped, G5 | S1 to S4, S7 | sum of sink deltas against final state minus initial state | equal, every sink |
| L1-6 | G4 | S7, S8 | observations that differ from `P(I(C))` | 0 |
| L1-7 | G2, G3 | S7, S8 | sinks whose cut sequence is not increasing per writer, or that show a write without an earlier write of the same writer | 0 |
| L1-8 | G1 | S7 | two runs with the same admission order that differ in any sink observation | 0 |
| L1-9 | static graph | S4 | operators created or destroyed after setup | 0 |
| L1-10 | G6 | S4 | cost of a write that changes a key divided by cost of one that changes a value, same row count | ≤ 3 |
| L1-11 | G6 | S5 | tick p50 at 10 times the state divided by tick p50 at reference state | ≤ 1.5 |
| L1-12 | all cores | S6 | tick p50 on 1 thread divided by tick p50 on 4 | ≥ 2.5, or the 1-thread p99 is already ≤ 3.3 ms |
| L1-13 | lifecycle | S9 | resident memory after returning to 0 keys, against the first time at 0 keys | within 5% |
| L1-14 | demand | S9 | rows processed per tick in the family at 0 demanded keys | 0 |
| L1-15 | setup | S1 | time to load reference state, and resident memory | ≤ 2 s, ≤ 1 GB |
| L1-16 | wasm | S10 | L1-5 to L1-8, and tick p99 | all 0, ≤ 33 ms |
| L1-17 | plane split | S11 | recompile time for a one-wire edit, and tick p99 during edits | ≤ 1 ms, and L1-1 still holds |
| L1-18 | G4 across planes | S11 | passes that evaluate with a schedule from a partly applied edit | 0 |
| L1-19 | plane split | S12 | tick p99 during the burst that the VM does not read | L1-1 still holds |
| L1-20 | nothing dropped, G2 | S13 | pulses seen by the sink against pulses sent, and out-of-order pulses | equal, 0 |

Today's comparison points for L1-1 and L1-15 are 32.7 ms of settle time per
tick, 60 s of setup and 9 GB, for the whole server. They are context, not
like-for-like.

### 9.3 Level 2: myko

In-process myko on the new engine, with a loopback WebSocket for the client
scenarios. This level exists to show that the level-1 numbers survive real
values.

Scenarios:

- **M1 store and query.** One entity type with 200,000 rows of a 512-byte
  structured payload (the load test's `structured_payload_bytes`) and a
  second with 4 KB rows. A tick writer updates 25,000 rows per tick at 30
  Hz. 12 subscribers each hold a `GetXsByQuery` with an `In` filter over
  5,400 ids (the load test's caps per client).
- **M2 view with a join.** A view joining two entity types on a foreign key,
  25,000 changed rows per tick on one side.
- **M3 report family.** One report type with 64,800 distinct params (12
  clients times 5,400), each a lookup and a map.
- **M4 probe.** A client command that writes one row and awaits the update
  on its own subscription, at 10 Hz, during M1 and during M3. Also idle.
- **M5 subscription churn.** 100 subscribes and 100 cancels per second
  during M1.
- **M6 atomic batch.** A grouped mutation across three entity types read by
  a view that joins all three.
- **M7 serialization.** M1 with JSON and with the binary wire format.
- **M8 shared params.** 12 clients subscribe to the same params.

Gates:

| gate | from | scenario | quantity | pass |
| --- | --- | --- | --- | --- |
| L2-1 | stability | M1, M2, M3 | tick completion p99 including serialization for all 12 subscribers | ≤ 10 ms |
| L2-2 | probe target | M4 | command to own subscription update, loopback, p99 loaded | ≤ 1.0 ms above the idle p99 of the same path |
| L2-3 | G7 | M4 | loaded p99 divided by idle p99 | ≤ 3.0 |
| L2-4 | nothing dropped | M1 to M5 | rows delivered per subscriber against rows expected, and per-subscription sequence gaps | equal, 0 gaps |
| L2-5 | G4 | M6 | subscriber messages that show some but not all of one mutation's types | 0 |
| L2-6 | value cost | M1 | per-row cost on 512-byte rows divided by the level-1 per-row cost | ≤ 4 |
| L2-7 | value cost | M1 | per-row cost on 4 KB rows divided by per-row cost on 512-byte rows | ≤ 1.5. Rows move by reference, so size must not matter much. |
| L2-8 | value cost | M1 | deep clones of a payload per operator it passes through | 0 |
| L2-9 | sharing | M8 | operator state for 12 identical subscriptions divided by state for 1 | ≤ 1.1 |
| L2-10 | sharing | M7 | serializations of one changed row for 12 subscribers with the same wire format | 1 |
| L2-11 | static graph | M5 | operators created after setup | 0 |
| L2-12 | G2, read-your-writes | M4 | command responses that arrive before the subscription update they caused | 0 |
| L2-13 | lifecycle | M5 | resident memory after 10 minutes of churn against after 1 minute | within 5% |
| L2-14 | setup | M1 | load of 200,000 rows | ≤ 5 s |

### 9.4 Level 3: rship-shaped load

The existing load test, unchanged in what it validates, at `reference-scale`
with 12 clients, plus one new probe.

Scenarios:

- **R1.** The e8 configuration: 30 Hz timers, 10 Hz emitter probe, 30 s
  measured after warm-up.
- **R2.** The e16 configuration, which today collapses to seconds of probe
  latency.
- **R3.** R1 with a second probe whose emitter is bound, through a binding
  graph in an active scene, to a target action. This is the shared path that
  the current probe does not exercise.
- **R4.** R1 for 30 minutes.

Gates:

| gate | from | scenario | quantity | pass |
| --- | --- | --- | --- | --- |
| L3-1 | probe target | R1, R2 | `emitter_rtt` p99 | ≤ 2.5 ms |
| L3-2 | probe target | R1, R2 | `emitter_rtt` max | < 7.7 ms |
| L3-3 | probe target | R1 | `emitter_rtt` p50 | ≤ 1.0 ms |
| L3-4 | probe target, G7 | R3 | pulse to executor action, p99 | ≤ 2.5 ms |
| L3-5 | nothing dropped | all | `missing_after_grace`; sent against received; feedback rows against 72,000 expected; every `*_verified` flag | 0; equal; equal; all true |
| L3-6 | stability | R1 | tick completion p99 in the server | ≤ 16.7 ms |
| L3-7 | stability | R2 | tick completion p99 | ≤ 25 ms |
| L3-8 | all cores | R1 | server cores used | ≤ 4.3, today's figure for the same work |
| L3-9 | lifecycle | R4 | resident memory at 30 minutes against at 2 minutes | within 5% |
| L3-10 | setup | R1 | `setup_seconds`, resident memory | ≤ 30 s, ≤ 9.1 GB |
| L3-11 | G4 | R1 | `subscribe_exec_events_with_retry` deleted and the exec-line tests pass | yes |

L3-11 is the end-to-end check of glitch-freedom: the repair code for a glitch
is removed and nothing breaks.

## 10. Uncertainty

- The rship classification is a sample read by delegates. The 39 `pure` node
  bodies were classified by trait, not by reading them.
- The 80,000-node cone and the half-of-tickets churn figure come from one
  dump and from the brief. I did not reproduce them.
- The census predates the `lookup` migration. The node count on `h4` may
  differ. The ratio of nodes to sites is unlikely to have changed in kind,
  because `family` still builds per key (`family.rs:199`).
- The per-row cost estimates are not measured.
- T3 is my construction. It has no proof and should not be built without
  one.
- PTP alignment was not found in the server, and `Pulse` carries no
  timestamp.
- The heights and the 13-node unit in section 5.1 are the coordinator's
  figures. The per-row costs in section 5.3 are estimates.
- The claim that a compiled schedule is the usual design for audio graphs,
  compositors and game engines is general knowledge, not checked.
- How much of the old graph is left after the VM alone is not known. The
  census by value type suggests most nodes are control-plane views.
- Whether reference edges in real binding graphs are acyclic is unknown.
- The 7.7 ms target is taken from the coordinator as given.

## 11. References

Verification status is in section 11.1.

- [Kahn] G. Kahn. The semantics of a simple language for parallel
  programming. IFIP Congress 1974.
- [B&A] J. D. Brock, W. B. Ackerman. Scenarios: a model of non-determinate
  computation. Formalization of Programming Concepts, LNCS 107, 1981.
- [CALM] J. M. Hellerstein, P. Alvaro. Keeping CALM: when distributed
  consistency is easy. Communications of the ACM 63(9), 2020.
- [Ameloot] T. J. Ameloot, F. Neven, J. Van den Bussche. Relational
  transducers for declarative networking. PODS 2011; Journal of the ACM
  60(2), 2013.
- [M&S] A. Margara, G. Salvaneschi. On the semantics of distributed reactive
  programming: the cost of consistency. IEEE Transactions on Software
  Engineering 44(7), 2018.
- [DREAM] A. Margara, G. Salvaneschi. We have a DREAM: distributed reactive
  programming with consistency guarantees. DEBS 2014.
- [SIDUP] J. Drechsler, G. Salvaneschi, R. Mogk, M. Mezini. Distributed
  REScala: an update algorithm for distributed reactive programming. OOPSLA
  2014.
- [TSRP] J. Drechsler, R. Mogk, G. Salvaneschi, M. Mezini. Thread-safe
  reactive programming. OOPSLA 2018.
- [Naiad] D. G. Murray, F. McSherry, R. Isaacs, M. Isard, P. Barham,
  M. Abadi. Naiad: a timely dataflow system. SOSP 2013.
- [Abadi13] M. Abadi, F. McSherry, D. G. Murray, T. L. Rodeheffer. Formal
  analysis of a distributed algorithm for tracking progress. FORTE 2013.
- [Brun21] M. Brun, S. Decova, A. Lattuada, D. Traytel. Verified progress
  tracking for timely dataflow. ITP 2021.
- [DD] F. McSherry, D. G. Murray, R. Isaacs, M. Isard. Differential
  dataflow. CIDR 2013.
- [Abadi15] M. Abadi, F. McSherry, G. D. Plotkin. Foundations of
  differential dataflow. FoSSaCS 2015.
- [SA] F. McSherry, A. Lattuada, M. Schwarzkopf, T. Roscoe. Shared
  arrangements: practical inter-query sharing for streaming dataflows. PVLDB
  13(10), 2020.
- [DBSP] M. Budiu, T. Chajed, F. McSherry, L. Ryzhyk, V. Tannen. DBSP:
  automatic incremental view maintenance for rich query languages. PVLDB
  16(7), pp. 1601-1614, 2023.
- [N&K] T. Neumann, A. Kemper. Unnesting arbitrary queries. BTW 2015, LNI
  P-241, pp. 383-402. Proof: T. Neumann. A formalization of top-down
  unnesting. arXiv 2412.04294, 2024.
- [Noria] J. Gjengset et al. Noria: dynamic, partially-stateful data-flow for
  high-performance web applications. OSDI 2018.
- Demand by memoised pull. Acar, Blelloch and Harper (Adaptive functional
  programming, POPL 2002) prove that change propagation yields the same
  result as a run from scratch. Adapton (Hammer, Phang, Hicks, Foster, PLDI
  2014) proves the same for a demand-driven calculus, where setting an input
  only marks edges dirty and repair happens when an observer forces a value.
  Salsa is the Rust engineering of that idea and states no theorem. These are
  the alternative to section 4.6. They fit a workload where most results are
  never read. rship's tick pushes 25,000 changes to 3,600 recipients that all
  read them, so pull saves nothing there.

### 11.1 Verification status

A delegate opened the paper text for each item unless noted, using generic
web searches. I did not re-read them.

Corrections to the citations as first recalled:

1. Kahn 1974 does not prove that execution equals the least fixed point. It
   defers to Cadiou.
2. The formal CALM result is Corollary 13 of Ameloot et al., about fair runs
   of transducer networks. The journal version is JACM 60(2), 2013.
3. Margara and Salvaneschi 2018 has five levels. The 2014 DREAM paper has
   three (causal, glitch free, atomic). The names do not line up between the
   two papers.
4. The OOPSLA 2018 scheduler is MV-RP, and its theorem is abort-free strict
   serializability.
5. Naiad has six authors.
6. The 2013 and 2021 progress-tracking proofs are safety only.
7. The 2013 differential dataflow paper defines traces over a partial order,
   but its algorithm and the Rust crate need least upper bounds. The 2015
   foundations paper needs local finiteness and no lattice. These are
   different hypotheses.
8. The DBSP paper has five authors. "Work proportional to the change" is not
   claimed for `MIN`-like aggregates.
9. The 2015 unnesting paper has no correctness proof.
10. Salsa has no formal statement.

Not opened, so unverified in content: the original Brock and Ackerman text
(read through Brock's thesis and a later report), Kahn and MacQueen 1977,
Acar's thesis, and the volume number of the CACM article.

Recalled by me and not checked this session: the serialization-graph theorem
behind T3 (Bernstein, Hadzilacos and Goodman; a search confirmed the
statement but not the attribution), vector clocks and causal delivery, and
magic sets.

Crate facts in section 6.2 are from a delegate that read the published
sources of `timely` 0.32.0, `differential-dataflow` 0.26.0 and `dbsp`
0.385.0, and the crates.io and GitHub records, on 2026-11-20.
