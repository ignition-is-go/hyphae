# Profiling collection work

The optional `profiling` feature emits TRACE events on `hyphae::work`:

- `map_general` and `map_single`: row-kernel calls in the corresponding query
  runtimes, including removals when those runtimes recompute old output rows.
- `map_values` and `filter_map_values`: stateless row-kernel calls.
- `pair_join_visit`: candidate visits in the incremental pair-join runtime.

Map labels are input value types; pair labels are paired value types. These
counters describe selected runtime kernels, not every collection operator or
all joins. A candidate visit does not guarantee a changed output. Capture them
with a consumer tracing subscriber and use existing `hyphae.fanout` spans for
cell emission counts. All new event code is cfg-elided without `profiling`.
Aggregation changes runtime cost and potentially scheduling; use separate
uninstrumented runs for CPU conclusions.
