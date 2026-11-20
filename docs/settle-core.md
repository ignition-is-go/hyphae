# Settle core

This document specifies how a write reaches subscribers. The implementation is
`hyphae/src/settle.rs`, with the node side in `cell.rs` and `source.rs`. The
behavior tests are in `hyphae/tests/settle_core.rs`.

Section 9 maps each ordering guarantee to the rule that keeps it. Section 10
says where the delivery envelope goes if the settle lock is ever split.

## 1. What this replaced

| Removed | Replaced by |
| --- | --- |
| Process-wide `TICK` mutex queue, `depth`/`draining` gate, height-wave drain | Input queue plus one settle lock (section 3) |
| Boxed `(write_value, fanout)` closures keyed by cell id | A typed pending list inside each node (section 4) |
| Last-write-wins coalescing of every notify | A combine rule per node (section 5) |
| `no_coalesce` scope, `Cell::no_coalesce`, `DepNode::no_coalesce` | Nothing. No notify is dropped by default |
| `Source::batched` | Nothing. Every settle is one pass |
| Wave-parallel drain, `WAVE_THRESHOLD`, `set_wave_threshold_for_test` | Nothing. One thread runs a settle |
| The eager path that ran each thread's write on that thread when no batch was open | Nothing. The settle core is the only propagation path, with or without the `scheduler` feature |

`join` and `join_vec` still keep their latest inputs behind a mutex. Inside a
settle they push the combined value to the output's pending list while they
hold it, and the push runs no subscriber. Outside a settle they queue a write
that reads the mutex when it is applied. No lock spans downstream work or a
waiting write.

The `scheduler` Cargo feature keeps its name. It now gates only `clock` and the
worker pool that map-query join regions use for their own computation.

Kept: the per-query FIFO gate in `map_query/compiler/dispatch.rs`. Every
entry into the sinks of one compiled query passes it: each root fanout, and
each per-row update of `map_values_cell` and `project_cell`. One thread at a
time runs a query's sinks, and an event that arrives meanwhile runs behind it
on that thread. The gate orders a subscribe-time replay, which runs on the
subscribing thread outside any settle, against a fanout from a settle. It
queues a root change that a sink makes during that replay. It also keeps two
nodes of one height, which fire at the same time (section 12), from running
one stateful sink on two threads. The gate serializes one query, not the
process.

## 2. Vocabulary

- **Node.** A `Cell` or a `Source`. A node has a height, a pending list, and
  subscribers.
- **Height.** `1 + max(height of each dependency)`. An input has height 0.
  Heights come from the guards a cell owns and are cached per node. An edge
  change invalidates the cache of the changed cell and of its dependents.
- **Write.** One signal addressed to one node.
- **Event set.** The unit a writer submits: one or more writes that settle
  together, zero or more exclusive keys, and an optional epoch.
- **Run.** The event sets that one settle combines.
- **Settle.** One height-ordered pass that delivers a run.
- **Holder.** The thread that holds the settle lock.
- **Stint.** What one holder settles between taking the lock and releasing it.

## 3. Input queue and settle lock

```rust
pub enum SettleKey { Shared, Exclusive(u64), Barrier }
pub struct Epoch(NonZeroU64);            // Epoch::next()

pub fn batch<R>(f: impl FnOnce() -> R) -> R;
pub fn batch_with<R>(keys: &[SettleKey], epoch: Option<Epoch>, f: impl FnOnce() -> R) -> R;
pub fn submit<R>(keys: &[SettleKey], epoch: Option<Epoch>, f: impl FnOnce() -> R) -> R;
pub fn enqueue<R>(keys: &[SettleKey], epoch: Option<Epoch>, f: impl FnOnce() -> R) -> R;
pub fn flush();
pub fn stats() -> SettleStats;
pub fn current_epoch() -> Option<Epoch>;
pub fn is_settling() -> bool;            // this thread is delivering a set
impl Epoch { pub fn scoped(finished: impl FnOnce() + Send + 'static) -> EpochScope; }

static INPUT: SegQueue<EventSet>;        // arrival-ordered, lock-free
static SETTLE: Mutex<()>;                // taken only with try_lock
static WAITING: Mutex<Vec<Arc<Waiter>>>; // writers asleep until settled
thread_local! { ACTIVE, BUILDING, MODE }
```

`ACTIVE` exists on a thread while it fires nodes: on the holder for its whole
stint, and on a settle pool thread while it fires part of a wave (section 12).
It carries the node now firing, the run id, the writers waiting on the current
run, and an outbox: the nodes that the thread's fires scheduled and the event
sets they queued. The holder alone owns the frontier and merges outboxes into
it. `BUILDING` exists while a thread is inside a batch and collects its
writes.

### How a write ends

A writer pushes its event set to `INPUT` and tries the lock.

- **The try succeeds.** The writer is the holder. It settles a stint. A
  `submit` caller then returns. Any other writer returns once its own set and
  the sets its callbacks queued have settled, which can mean sleeping while
  another holder settles the rest.
- **The try fails.** What happens next depends on the entry point:

| Entry point | When the lock is held by another thread |
| --- | --- |
| `set`, `emit`, a collection mutation, `batch`, `batch_with` | The writer sleeps until its event set has settled. It can then read its own write back |
| `submit` | The writer returns at once. The holder delivers the set |
| `enqueue` | The writer returns at once, and it does not try the lock at all |
| A write from a callback on the holder | Never waits. See section 6 |

A sleeping writer waits on its own set and on every set that a callback wrote
while that set was settling. It therefore wakes to the settled result of its
write, including feedback.

`submit` is for a thread that must not stall, such as a network reader or the
timer reactor. Hyphae's own timer reactor uses it. When the lock is free,
`submit` settles on the calling thread, so subscriber callbacks run there.

`enqueue` runs no callback on the calling thread. It is the form to use while
holding a lock that a subscriber callback takes: `enqueue` under the lock,
release the lock, then `flush`. A waiting write there would deadlock against
the holder, and `submit` there would deadlock against itself when it becomes
the holder. `flush` waits until everything queued before it has settled.

### Stints and hand-over

A holder settles the event sets that were queued when it took the lock. Then
it releases the lock. The bound has no exception. A set that a callback queues
during the stint stays in `INPUT`, behind every set that arrived before it,
and a later stint settles it. `INPUT` is one arrival-ordered queue, so
settling a callback's set inside the stint would mean settling every other
thread's set ahead of it, and under sustained feedback the stint would never
end.

If sets are queued when the stint ends, somebody must settle them. The holder
asks one sleeping writer to take over and leaves. If no writer is asleep, it
yields and tries the lock again. It never wakes a thread that is not already
waiting on its own write. This keeps one thread, in particular a thread that
used `submit`, from settling for the whole process while a waiting writer can
do it. When every other writer uses `submit`, nobody is asleep, and the holder
keeps settling until the queue is empty or another writer takes the lock in
the gap between two stints.

Nothing is stranded: a holder checks the queue after it releases, a writer
that lost the try pushes and then tries once more, and a writer that was asked
to take over always settles one stint, even if its own set finished meanwhile.

### Splitting a stint into runs

The holder walks the popped sets in arrival order and adds each to the current
run. It closes the run and starts a new one before adding a set when:

- the set carries an exclusive key that the run already holds, or
- the set is a `Barrier`, or the run holds a `Barrier`, or
- the set's epoch differs from the run's. A set without an epoch differs
  from a set with one. An empty set, which `flush` queues, fits any run.

A run therefore has one epoch or none. While it settles,
`settle::current_epoch()` returns that epoch in every callback, on the
holder and on a settle pool thread. An event set that a callback queues
carries the epoch on unless the callback passes its own, so a feedback chain
keeps the epoch of the write that started it. A caller uses this to
recognize what a write caused without keeping state in a thread-local,
which a callback on another thread would not see. Event sets without an
epoch combine exactly as they did before epochs were read.

The writes of an epoch can settle after their writer has returned: `submit`
leaves them queued, and feedback takes further runs. `Epoch::scoped`
allocates an epoch with a callback and returns a scope. The callback runs
once, when the scope has been dropped and every event set that carries the
epoch has settled: a batch still collecting writes, queued sets, and the
sets that callbacks queued under it. It runs on the thread that drops
the scope or on the thread that settles the last set. There it runs after
the run, with no epoch, and a write it makes is queued. A caller keeps its
per-epoch state until then.

Runs settle in order, so arrival order is preserved across runs. A hash
collision between two keys can only cause an extra split.

| Write | Key |
| --- | --- |
| `Cell::set`, `complete`, `fail`, collection mutations | `Shared` |
| `Source::emit`, `Source::complete` | `Exclusive(hash of the source id)` |
| `batch(f)` | The keys of the writes inside it |
| `batch_with`, `submit`, `enqueue` | The caller's keys, plus the keys of the writes inside |

So two emits on one `Source` from two event sets never share a settle, and two
sets of one `Cell` may.

## 4. Nodes

```rust
pub trait Node: Send + Sync {            // doc(hidden)
    fn node_id(&self) -> Uuid;
    fn node_height(&self) -> u64;
    fn claim(&self) -> bool;             // take the one frontier place
    fn fire(&self);                      // deliver everything pending
    fn bump_height_epoch(&self);
    fn node_dependents(&self) -> Vec<Weak<dyn Node>>;
}

struct CellInner<T> {
    // id, subscribers, result_subscribers, value, name, owned, completed,
    // errored, error, caller: unchanged
    pending: Mutex<Vec<Signal<T>>>,      // empty between settles
    queued: AtomicBool,                  // holds a frontier place
    combine: OnceLock<Combine<T>>,
    fired_run: AtomicU64,                // feeds the duplicate-fire counter
    height_cache: AtomicU64,
    height_epoch: AtomicU64,
    dependents: Mutex<Vec<Weak<dyn Node>>>,
}
```

`SourceInner<T>` has `pending`, `queued`, and `fired_run`. A source has height
0 and stores no value.

`Signal<T>` and every subscriber signature are unchanged.

A settle seeds each write of its run into the target's pending list and puts
the target in the frontier. It then pops the frontier, lowest height first and
earliest arrival first within a height. Firing a node takes its pending list
and, for each signal in order, stores the value or terminal state and runs the
fanout. A notify that a callback makes goes to its target's pending list.

A queued write holds its cell weakly. A cell that nothing else holds has no
subscriber left to tell, so its write is dropped.

## 5. Combine rules

A node can receive more than one value in a settle. The rule says what the
second one does to the pending one.

```rust
enum Combine<T> {
    Each,                                             // deliver both
    Latest,                                           // keep the last
    Fold(fn(&mut Arc<T>, Arc<T>) -> Option<Arc<T>>),  // fold into one
}
```

| Node | Rule | Reason |
| --- | --- | --- |
| A `Cell` written through `Mutable::set`, when the write goes through the input queue: from outside a settle, or deferred from a callback (section 6) | `Latest` | State. Queued sets of one cell that share a settle leave the last value |
| A `Cell` written through `notify`, or through `set` from a callback whose write joins the settle in progress | `Each` | The operator may carry events. Nothing is dropped |
| The output of `join`, `join_vec`, and `switch_map` | `Latest` | Each input's callback pushes the full combined state under the operator's state lock, so the last push holds every settled input. This is the glitch-free join |
| The funnel cell inside `merge`, `zip`, and `take_until` | `Each` | Both sources push their signals into one cell, and the operator's logic runs when that cell fires. See "Order within a pending list" |
| `CellMap` diff cell | `Fold(MapDiff::compose)` | Diffs of one settle become one `MapDiff::Batch`. The stream is consistent and ends at the stored data: each diff starts from what subscribers hold. A mutation made outside a settle is told as one diff per key per event set. See "Collection diffs". The fold runs when the cell fires |
| `CellSet` diff cell | `Each` | `SetDiff` has no batch form. The stream is consistent and ends at the stored data, as for `CellMap` |
| `Source` | Each | Events |

A terminal signal never combines with a value. `Latest` and `Fold` touch only a
trailing value, so a value followed by `Complete` keeps both, in that order.

### Order within a pending list

Nodes of one height can fire at the same time (section 12), so values from
two nodes can reach a third in either order. Each pending value carries a
stamp: the height and frontier position of the node that pushed it. Stamps
order the fires of one settle the way one thread would have run them.

- **An event, written with `notify`,** is delivered in stamp order. The cell
  sorts its pending list when it fires. Values that one node pushed share a
  stamp and keep their arrival order. This covers `merge`, `zip`,
  `take_until`, and any operator that forwards with `notify`.
- **State, written with `Mutable::set`,** keeps its arrival position. The
  writer changed its own state before it wrote, and the values must follow
  that order. The diffs of a `CellMap` or `CellSet` are state of this kind.
  "Collection diffs" says how they are ordered.
- **A terminal signal** keeps its arrival position too. Its writer ends the
  stream after the values it has seen, so no value that arrived before it is
  delivered after it.
- **A `Latest` cell** keeps arrival order. Its writers push under their state
  lock, so the last arrival holds the most.

### Collection diffs

A `CellMap` or `CellSet` has one write lock. A mutation holds it from the
change of the stored data until its writes are in a pending list or in the
input queue. A settle holds it while it applies a publish request. No
subscriber runs under it, and no thread waits for a settle while it holds it.
The implementation is `hyphae/src/cell_map/publish.rs`.

How a mutation tells subscribers depends on whether it joins a settle.

- **It joins the settle in progress.** The diff cell has not fired in this
  settle (section 6). The mutation writes the key cells, the diff, and the
  length at once. These diffs are in mutation order. When two nodes of one
  height write one key in one settle, which write is last is not specified.
- **It does not join.** This is every mutation from outside a settle,
  including one inside `batch`, `submit`, or `enqueue`, and a mutation from
  a callback whose diff cell has already fired. The mutation changes the
  stored data and queues no diff. For each key it changes, the collection
  records the value subscribers were last told, and the mutation adds one
  publish request for those keys to the thread's event set. When a settle
  seeds the request, the collection emits, for each key, the diff from the
  value subscribers were told to the value now stored. It then writes the key
  cell and the length from the stored data.
- **It joins, but changes a key that has a publish request queued.** It
  queues a publish request too. A diff written at once would start from the
  stored value, which subscribers do not hold yet.

An event set reaches the input queue when it closes, which can be after
another thread's later mutation. A diff built at mutation time would then
arrive out of order. A publish request carries no values, so the order in
which requests arrive does not matter.

A thread can settle while it has a batch of its own open: it called `flush`
inside the batch. A write it makes then waits in that batch. A publish
request that such a thread seeds therefore tells nothing yet. It moves into
the thread's batch and is applied when that batch settles.

A subscriber can rely on this:

1. Every diff starts from what the subscriber holds. The `old_value` of an
   `Update` or `Remove` is the value it was last told, an `Insert` names a key
   it does not hold, and a `SetDiff` changes the membership it holds.
2. Once every queued event set has settled, the subscriber holds the stored
   data, and so do the key cells and the length.

A subscriber cannot rely on one diff per mutation, for a mutation that does
not join a settle:

- One event set tells one diff per key. Values in between are not told. An
  insert and a remove of one key in one `batch` tell nothing.
- A diff can carry another thread's later change to the key. That thread's
  request then tells nothing for the key.
- A diff applied with `apply_diff_owned` or `apply_batch` is told as the step
  the stored data took. An `Insert` over an existing key is told as an
  `Update`, and a `Remove` of an absent key tells nothing.
- A mutation that emits `MapDiff::Initial` still does. The entries are the
  ones stored when the request is applied.

A key cell or a membership cell created while a change to its key waits to be
published starts at the value subscribers were told, so the cell and the
diffs describe one state. The length is written only when it differs from
the length last written.

The cost for a mutation that does not join a settle is one hash insert and
one lookup per changed key. A mutation that joins pays one check that no key
has a request queued.

`MapDiff::compose` appends to a pending `Batch`, flattening a nested one.
Otherwise the pending diff becomes `Batch { changes: [pending, next] }`. An
`Initial` snapshot is never placed in a batch. A composed batch can hold
several changes to one key. The map runtimes already accept that shape.

No node suppresses an equal value by default. Earlier work showed that
suppressing equal values breaks event-like cells, and equal writes are under
1% of the measured load.

A cell is state, and a source is events. One thread's consecutive writes to a
cell never share a settle, because each returns settled before the next
starts. Writes from different threads, writes inside one batch, and sets that
callbacks deferred during one settle can. A caller that needs every one of
those delivered uses a `Source`, writes the cell with `notify`, or gives the
event sets an exclusive key.

`Latest` on a fan-in output applies to every value, however it was written. An
event that passes through `join`, `join_vec`, or `switch_map` survives only if
no other value of the same output shares its settle. Give such events an
exclusive key so that each one settles alone.

Collections: a `CellMap` mutation from outside a settle is one event set that
holds one publish request ("Collection diffs"). `CellMap::own` records the
guard's source as a dependency of the diff cell, and the length cell and each
key cell depend on the diff cell. A derived map therefore sits above its
inputs, and what it publishes from a callback joins the settle in progress.
A materialized map is built holding the rows its roots replay; the replay
publishes no diff.

## 6. Reentrancy rule

A callback that runs during a settle may write to a node. Let `h` be the height
of the node now firing and `t` the height of the target.

- **`t > h`: the write joins the current settle.** The target has not fired in
  this settle, so it fires after every node below it. This is how operators
  propagate.
- **`t <= h`: the write becomes a new event set at the tail of `INPUT`.** A
  later stint settles it, on this holder or on the next one. Every writer that
  waits on the current run waits on this set too.
- **A `batch`, `batch_with`, `submit`, or `enqueue` opened inside a settle**
  becomes one new event set at the tail of `INPUT`, whatever the heights of
  its writes. Its writes stay together.
- **A node with a queued write queues the next one too.** While a write that
  a callback queued for a node has not been seeded, a later write to that
  node from a callback goes to the tail of `INPUT` as well, even when
  `t > h`. Heights are not fixed: a node that gains a dependency rises. A
  cell written at height 0 and then given a dependency in the same settle
  would otherwise take its second write into the current settle, ahead of
  the first. Each node counts its queued callback writes for this.

The rule depends on structure, not on which method made the write. A derived
collection that publishes through `insert` and a hand-written operator that
calls `notify` both join the current settle when their output depends on the
firing node. A callback that feeds a result back into an input always starts a
later settle.

A callback that writes and then reads the same node reads the old value when
the write was deferred.

A callback never waits, so a feedback write cannot deadlock the holder.

### Edge check

A write that joins must also travel a declared edge: the target must be
reachable from the firing node through the dependents that `own` and
`own_keyed` register. Pipelined tickets (`docs/settle-epochs.md`, section 6)
cannot let an undeclared write join its ticket. Debug builds check this on
every joined write and panic on one with no edge, naming the creation sites
of both nodes. `HYPHAE_EDGE_CHECK=warn` logs each pair of sites once instead,
and `HYPHAE_EDGE_CHECK=off` turns the check off. Release builds compile it
out. A write meant to start its own ticket goes inside `settle::new_ticket`,
which exempts it.

hyphae declares every internal edge where the node is built, before the node
can receive a live write: prepared pipeline installs, `CellMap` outputs, and
`SharedMapQuery` consumers.

## 7. What a caller can observe

- A write through `set`, `emit`, a collection mutation, or `batch` is settled
  when it returns, including the feedback it caused.
- Inside a batch, a read sees the value from before the batch.
- A subscriber callback runs on whichever thread holds the settle lock, or
  on a settle pool thread when waves are on (section 12). It is not always
  the writer's thread.
- A callback that blocks or computes for long delays every write in the
  process until it returns. Debug builds log a warning that names the cell
  when one fanout holds the settle for more than a second.
- A panic in a callback does not stop the settle. When every event set of the
  run has a sleeping writer, the first panic is re-raised on one of them.
  Otherwise it is re-raised on the holder after it releases the lock.

## 8. Hazard: a lock held across a write

Before this core, a thread's write ran its fanout on that thread. A writer
could hold a reentrant lock across the write and take it again in a callback.

Now another thread may run that callback. If the writer sleeps until its write
has settled while it holds a lock the callback needs, the two deadlock. The
rule for callers: do not hold a lock across a waiting write if any subscriber
callback takes that lock. Use `enqueue` under the lock and `flush` after it.

One process-wide lock also couples unrelated graphs in time. A timer's write
can sit behind another graph's settle. Tests that asserted a wall-clock window
of a few tens of milliseconds had to allow for that.

## 9. Ordering contract

| Guarantee | How the core keeps it |
| --- | --- |
| G1 Glitch-free settle | Height-ordered frontier. `Latest` on fan-in outputs |
| G2 Batch atomicity | An event set is never split across runs. Collection diffs travel as one `Batch` |
| G3 Last-write-wins | Removed, except `Latest` on input cells and fan-in outputs |
| G4 Per-node write order | `INPUT` is arrival-ordered and runs settle in order. A pending list keeps arrival order. A deferred write goes to the tail of `INPUT`, behind every write made before it, and two deferred writes to one node keep their order. A write that joins the current settle lands behind the node's earlier pending values. A callback's write cannot join a settle ahead of an earlier callback write to the same node that is still queued: the node queues it behind that one (section 6) |
| G5 Join consistency | One thread settles. A join output fires once, after its inputs |
| G6 `Cell` replays, `Source` does not | `subscribe` is unchanged. It replays the stored value, which is the settled value. A `Source` stores none |
| G7 Gap-free `subscribe_diffs` | The subscribe, snapshot, buffer, replay sequence is unchanged. Diffs reach the diff cell in write order and compose in that order |
| G8 Terminal order | A terminal never combines with a value and the pending list keeps order |
| G9 Convergence under concurrent writers | Every write passes through `INPUT` and one settle at a time, so two writers' fanouts cannot interleave |

## 10. Where the envelope goes when the lock is split

Today an event set is settled whole inside one run, and a join fires after
every node below it. A join therefore always has every input that a
multi-node write touched. The epoch is kept per run (section 3), not per
pending value.

If the lock is split by partition, the parts of one write can settle at
different times, and a join can fire between them. The envelope goes in these
places:

1. **Pending list.** `CellInner::pending` holds `Signal<T>`. It becomes
   `Delivery<T>`: `Changed(Arc<T>, Option<Epoch>)`, `EpochOnly(Epoch)`,
   `Complete`, `Error`. `Signal<T>` and the subscriber signatures stay as
   they are.
2. **Seeding.** `Write::apply` takes the event set's epoch and tags the
   `Changed` it pushes.
3. **Inheritance.** `ACTIVE` gains the epoch of the delivery now firing. A
   notify that joins the settle copies it. This makes `map` and `filter`
   carry the tag with no change to either.
4. **Epoch-only forwarding.** After a tagged fanout, `fire` sends
   `EpochOnly` to each entry of `dependents` that the fanout did not reach.
   A node whose pending list holds only `EpochOnly` runs no callback and
   forwards it. This covers a rejecting `filter`, `deduped`, and a no-op
   collection write without touching them.
5. **Joins.** A `Latest` node records, per input, the last epoch it saw. On a
   tagged delivery it holds its output until every input the epoch touched
   has delivered `Changed` or `EpochOnly`, then fires untagged. The set of
   touched inputs comes from the event set, so `EventSet` gains the list of
   nodes it writes.
6. **Sinks.** A subscriber that is not a node receives nothing for
   `EpochOnly`.

The cost is one walk of `dependents` per tagged fanout. Restrict it to paths
that reach a `Latest` node if that walk shows up in a profile.

## 11. Counters

`settle::stats()` returns process-wide totals since start: event sets
submitted, runs settled, fanouts, duplicate fanouts (a node that fanned out
more than once in one run), lock misses, lock waits, deferred writes, and
nanoseconds the settle lock was held, the longest single hold, hand-overs,
empty event sets queued by `flush`, waves fired, waves fired on pool threads,
the most nodes in one wave, and epoch splits: the times an event set started
a new run only because its epoch differed.
They are relaxed atomics and always on.

## 12. Parallel settling

### What was measured

The load harness at 8 elements per scene, after the fixes of sections 3
and 5 (`.audit/logs/cones-e8.txt`, one run, with the cone dump on):

- A run is one of two things. Half of all runs fire 1 node. One run per
  timer tick fires about 25,800 nodes. The tick is one event set, and
  everything it reaches settles in one height-ordered pass.
- That one pass takes longer than the 33 ms tick period, so ticks are
  skipped and executors see 8 to 9 updates a second out of 30. The input
  queue is empty meanwhile (backlog peak 1).
- **Lanes over event sets with disjoint cones gain nothing.** If a run locked
  the nodes it fires and runs with a common node were serial, the speedup
  bound is 1.00 to 1.01, with or without the 1,024 hottest nodes cut out as
  boundaries. The work is inside single runs, not spread over many.
  An earlier census on v3.1.12 found the same shape from the other side: 90%
  of live server cells are one connected component.
- **Firing the nodes of one height concurrently gains nearly linearly.**
  With unit cost per fire, the bound is 1.98, 3.87, 7.41, and 13.79 at 2, 4,
  8, and 16 threads.

### Design: wave-parallel settle

Heights already give the partition. Two nodes of equal height never depend on
each other, so a settle can fire them at the same time and still fire every
node after all of its dependencies.

- **Wave.** The holder pops every frontier entry of the lowest height. That
  list is a wave. A fire schedules only nodes above its own height, so the
  wave is complete when it is popped.
- **Switches.** `HYPHAE_SETTLE_WORKERS` is the number of threads that fire a
  wave. `HYPHAE_SETTLE_WAVE_MIN` is the smallest wave that leaves the holder;
  the default is 64. Both are read once. `settle::set_workers` and
  `settle::set_wave_min` override them for tests.

  | `HYPHAE_SETTLE_WORKERS` | A wave of at least `WAVE_MIN` nodes |
  | --- | --- |
  | 0, the default | Fires on the holder. So does every smaller wave, at any setting |
  | 1 | Fires on one pool thread while the holder waits. With `WAVE_MIN=1` every callback runs off the writer's thread, one at a time. This is the mode that exposes a thread-affine callback or a lock held across a write without any concurrency |
  | n, 2 or more | Fires on n threads. The holder is one of them. A wave of one node fires on the holder |

- **Workers.** The wave is split into chunks that the threads claim through
  one atomic index: eight chunks per thread, 256 nodes at most, so a thread
  that drew cheap nodes takes over part of a slow thread's share. The settle
  moves to the next height when every node of the wave has fired. The settle
  lock, the input queue, runs, stints, and waiters do not change.
- **Pool.** The pool threads are plain threads named `hyphae-settle-N`,
  started on first use. They are not the Rayon pool of `executor.rs`. A
  Rayon thread that blocks inside a callback, as a parallel join region does,
  runs other queued jobs on the same stack. A second node would then fire
  inside the first node's callback and under its locks.
- **Worker context.** Each pool thread gets its own `ACTIVE` for the wave:
  the node now firing, the run id, the waiter list, and an outbox. The
  reentrancy rule of section 6 is evaluated on the worker exactly as on the
  holder, and a callback there never waits.
- **A write that joins the settle** goes to the target's pending list under
  the target's own lock, and leaves a request for the target in the firing
  thread's outbox. After the wave the holder walks the outboxes chunk by
  chunk in wave order. The first request for a node claims its `queued` flag
  and enters the frontier with the next `seq`; later requests find the flag
  taken. A node therefore enters the frontier at its first write in the
  order of one thread, although a later node of the wave may have written to
  it first in time. The next wave's order, and every stamp (section 5), is
  the same as on one thread. A node gives the flag up when it fires.
- **A write that is deferred** goes to the same outbox. The holder appends
  the event sets to `INPUT` in wave order. Deferred sets therefore reach
  `INPUT` in the order a single thread would have produced.
- **Panics.** Caught per node. The first in wave order is kept, whichever
  thread panicked first.
- **A wave on the holder** uses the same path with one outbox, merged after
  the wave.

### What changes for a caller

- Callbacks of different nodes can run at the same time, on pool threads.
  Callbacks of one node never overlap. v3.1.12 behaved this way above its
  wave threshold.
- A callback can run on a thread that is not the writer's even when the
  writer took the lock. The rule of section 8 loses its exception: a lock
  held across any write deadlocks if a callback takes it. A reentrant lock
  does not help.
- Within one settle, two nodes of equal height reach a shared target in an
  order that depends on timing. The target delivers events in the order of a
  one-thread settle all the same (section 5). State writes keep arrival
  order.

| Guarantee | Under waves |
| --- | --- |
| G1, G5 glitch-free joins | Kept by height order. A `Latest` fan-in pushes its combined value while it holds its own state lock, so the last push carries every settled input. `join`, `join_vec`, and `switch_map` do this |
| G2 batch atomicity | Unchanged. An event set is still one run |
| G4 per-node write order | Kept. Events from two sources of equal height are delivered in the order of a one-thread settle, by the stamp on each pending value (section 5). `merge`, `zip`, and `take_until` run their logic in that order, on one thread. A collection mutation that joins a settle has its diff delivered in mutation order. A mutation made outside a settle queues no diff of its own: the settle that applies its publish request tells the step from what subscribers hold to what is stored, so the stream is consistent and ends at the stored data in whatever order the event sets arrive (section 5, "Collection diffs"). Which of two equal-height nodes writes one `CellMap` key last is unspecified; the diffs still match the stored data |
| G7 gap-free `subscribe_diffs` | Kept for a mutation that joins a settle: it writes its diff while it holds the collection's write lock, so diffs compose in mutation order. A mutation made outside a settle is told when its publish request is applied, as the step from what subscribers hold to what is stored. A subscriber that subscribes while such a request is queued reads the change in its snapshot and is then told it again; replaying both still gives the stored data |
| One compiled map query | One thread at a time runs its sinks. Root fanouts and per-row updates pass one gate |
| Read-your-writes for a waiting writer | Unchanged. The waiter counts sets, not threads |
| Feedback ordering | Kept by the per-node buffer and the wave-order append |
| G9 convergence | Unchanged |

A rewire during a wave can raise the height of a node that another worker is
already firing. That node then fires again when its new dependency fires. The
result converges, and the extra fire shows in `duplicate_fires`.

### Open points that need a ruling

1. Waves replace the lanes that were asked for. The measurement above is the
   reason.
2. Concurrent callbacks are a contract change. myko and rship must be clean
   of locks held across writes before waves can be on by default. One such
   site is known and blocks the harness at this load today:
   `MykoServerContext::typed_projection` holds a cache shard while
   `typed_map_from_any_item_with_typed_id` makes a waiting write.
3. Whether unspecified order between equal-height sources is acceptable for
   `merge`. The alternative is a sequence stamp per pending value, sorted at
   fire. Ruled: the stamp. Section 5 describes it.

### Pieces

Each piece has one owner and its own files. Piece 1 lands first because it
defines the switches the others test against.

| # | Piece | Files | Check |
| --- | --- | --- | --- |
| 1 | Switches and foreign-thread mode: `HYPHAE_SETTLE_WAVE_MIN`, `HYPHAE_SETTLE_WORKERS`, and matching test setters. With `WAVE_MIN=1` and one worker, every node fires on a pool thread, one at a time. No concurrency yet, but every thread-affine callback and every lock held across a write shows up | `hyphae/src/settle.rs`, `hyphae/src/executor.rs` (the rayon pool behind `worker_pool()` already exists under the `scheduler` feature) | hyphae suite in both modes |
| 2 | Concurrent waves: chunk claiming, worker `ACTIVE`, frontier merge, deferred-set buffer, panic order, counters (waves, parallel waves, widest wave) | `hyphae/src/settle.rs`, new `hyphae/src/settle/wave.rs`, new `hyphae/tests/settle_wave.rs` | new tests; hyphae suite with `WAVE_MIN=1 WORKERS=8`, 3 runs |
| 3 | Operators under waves: `Latest` fan-ins push under their state lock; audit `merge`, `MapDiff::compose`, the map-values queue, and the map-query FIFO gate | `hyphae/src/traits/operators/`, `hyphae/src/traits/collections/internal/`, `hyphae/src/map_query/compiler/dispatch.rs` | operator and map-query tests with `WAVE_MIN=1 WORKERS=8` |
| 4 | myko clean under foreign threads: fix `typed_projection`, `compute_or_cache`, and whatever piece 1's mode finds | `myko/libs/myko/core/src/server/context.rs`, `core/query/`, `core/graph.rs` | myko serial and parallel suites with piece 1's mode on |
| 5 | rship clean under foreign threads | `rship/libs/entities/` crates that subscribe | the crate suites of unit B with piece 1's mode on |
| 6 | Tuning: threshold and worker count from the harness | harness only | below |

Pieces 3, 4, and 5 need only piece 1. Piece 2 needs piece 1. Piece 6 needs
all.

Harness after each piece, new core against baseline, interleaved, 5 runs a
side, reporting RTT p50 and p99, delivered callbacks per second, backlog,
enqueue-to-run latency, and longest hold:

- After piece 4: 2 and 8 elements per scene with waves off. This is the first
  clean 8-element number; setup deadlocks 2 runs in 2 without it.
- After pieces 2 and 3: 8 and 16 elements with workers at 1, 4, 8, and 16.
- After piece 5 and after piece 6: 2, 8, and 16 elements at the chosen
  setting.
