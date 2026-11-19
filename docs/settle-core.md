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

`join` and `join_vec` still keep their latest inputs behind a mutex, but they
release it before they notify, and a notify inside a settle only queues. No
lock spans downstream work.

The `scheduler` Cargo feature keeps its name. It now gates only `clock` and the
worker pool that map-query join regions use for their own computation.

Kept: the per-query FIFO gate in `map_query/compiler/dispatch.rs`. A settle
already serializes root fanouts that come from writes. The gate still does
two things the settle lock does not: it orders a subscribe-time replay, which
runs on the subscribing thread outside any settle, against a fanout from a
settle, and it queues a root change that a sink makes during that replay.

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

static INPUT: SegQueue<EventSet>;        // arrival-ordered, lock-free
static SETTLE: Mutex<()>;                // taken only with try_lock
static WAITING: Mutex<Vec<Arc<Waiter>>>; // writers asleep until settled
thread_local! { ACTIVE, BUILDING, MODE }
```

`ACTIVE` exists only on the holder and carries the frontier, the height of the
node now firing, and the writers waiting on the current run. `BUILDING` exists
while a thread is inside a batch and collects its writes.

### How a write ends

A writer pushes its event set to `INPUT` and tries the lock.

- **The try succeeds.** The writer is the holder. It settles a stint and
  returns with its own write delivered.
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

A holder settles the event sets that were queued when it took the lock, plus
every set its own callbacks queue. Then it releases the lock.

If sets arrived meanwhile, somebody must settle them. The holder asks one
sleeping writer to take over and leaves. If no writer is asleep, it yields and
tries the lock again. It never wakes a thread that is not already waiting on
its own write. This keeps one thread, in particular a thread that used
`submit`, from settling for the whole process while others keep writing.

Nothing is stranded: a holder checks the queue after it releases, a writer
that lost the try pushes and then tries once more, and a writer that was asked
to take over always settles one stint, even if its own set finished meanwhile.

### Splitting a stint into runs

The holder walks the popped sets in arrival order and adds each to the current
run. It closes the run and starts a new one before adding a set when:

- the set carries an exclusive key that the run already holds, or
- the set is a `Barrier`, or the run holds a `Barrier`.

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
    fn fire(&self);                      // deliver everything pending
    fn bump_height_epoch(&self);
    fn node_dependents(&self) -> Vec<Weak<dyn Node>>;
}

struct CellInner<T> {
    // id, subscribers, result_subscribers, value, name, owned, completed,
    // errored, error, caller: unchanged
    pending: Mutex<Vec<Signal<T>>>,      // empty between settles
    queued: AtomicBool,                  // in the frontier
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
| A `Cell` written through `Mutable::set` from outside a settle | Latest | State. Sets of one cell that share a settle leave the last value |
| A `Cell` written through `notify`, or from a callback inside a settle | Each | The operator may carry events. Nothing is dropped |
| The output of `join`, `join_vec`, and `switch_map` | `Latest` | Each input's callback emits the full combined state, so the last one holds every settled input. This is the glitch-free join |
| `CellMap` diff cell | `Fold(MapDiff::compose)` | Diffs of one settle become one `MapDiff::Batch`, in write order. None is dropped |
| `CellSet` diff cell | `Each` | `SetDiff` has no batch form |
| `Source` | Each | Events |

A terminal signal never combines with a value. `Latest` and `Fold` touch only a
trailing value, so a value followed by `Complete` keeps both, in that order.

`MapDiff::compose` appends to a pending `Batch`, flattening a nested one.
Otherwise the pending diff becomes `Batch { changes: [pending, next] }`. An
`Initial` snapshot is never placed in a batch. A composed batch can hold
several changes to one key. The map runtimes already accept that shape.

No node suppresses an equal value by default. Earlier work showed that
suppressing equal values breaks event-like cells, and equal writes are under
1% of the measured load.

A cell is state, and a source is events. One thread's consecutive writes to a
cell never share a settle, because each returns settled before the next
starts. Writes from different threads, and writes inside one batch, can. A
caller that needs every one of those delivered uses a `Source`, or gives the
event sets an exclusive key.

Collections: a `CellMap` mutation is one event set. `CellMap::own` records the
guard's source as a dependency of the diff cell, and the length cell and each
key cell depend on the diff cell. A derived map therefore sits above its
inputs, and what it publishes from a callback joins the settle in progress.

## 6. Reentrancy rule

A callback that runs during a settle may write to a node. Let `h` be the height
of the node now firing and `t` the height of the target.

- **`t > h`: the write joins the current settle.** The target has not fired in
  this settle, so it fires after every node below it. This is how operators
  propagate.
- **`t <= h`: the write becomes a new event set at the tail of `INPUT`.** The
  holder settles it in a later run of the same stint, before it releases the
  lock.
- **A `batch`, `batch_with`, `submit`, or `enqueue` opened inside a settle**
  becomes one new event set at the tail of `INPUT`, whatever the heights of
  its writes. Its writes stay together.

The rule depends on structure, not on which method made the write. A derived
collection that publishes through `insert` and a hand-written operator that
calls `notify` both join the current settle when their output depends on the
firing node. A callback that feeds a result back into an input always starts a
later settle.

A callback that writes and then reads the same node reads the old value when
the write was deferred.

A callback never waits, so a feedback write cannot deadlock the holder.

## 7. What a caller can observe

- A write through `set`, `emit`, a collection mutation, or `batch` is settled
  when it returns, including the feedback it caused.
- Inside a batch, a read sees the value from before the batch.
- A subscriber callback runs on whichever thread holds the settle lock. It is
  not always the writer's thread.
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
| G4 Per-node write order | `INPUT` is arrival-ordered and runs settle in order. A pending list keeps arrival order. A deferred write goes to the tail of `INPUT`, behind every write made before it, and two deferred writes to one node keep their order. A write that joins the current settle lands behind the node's earlier pending values. A node cannot receive a joined write after a deferred one in the same settle: joining needs `t > h`, deferring needs `t <= h`, and `h` never decreases within a settle |
| G5 Join consistency | One thread settles. A join output fires once, after its inputs |
| G6 `Cell` replays, `Source` does not | `subscribe` is unchanged. It replays the stored value, which is the settled value. A `Source` stores none |
| G7 Gap-free `subscribe_diffs` | The subscribe, snapshot, buffer, replay sequence is unchanged. Diffs reach the diff cell in write order and compose in that order |
| G8 Terminal order | A terminal never combines with a value and the pending list keeps order |
| G9 Convergence under concurrent writers | Every write passes through `INPUT` and one settle at a time, so two writers' fanouts cannot interleave |

## 10. Where the envelope goes when the lock is split

Today an event set is settled whole inside one run, and a join fires after
every node below it. A join therefore always has every input that a
multi-node write touched. The `epoch` argument is accepted and stored on the
event set, and nothing reads it.

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
nanoseconds the settle lock was held, the longest single hold, and hand-overs.
They are relaxed atomics and always on.
