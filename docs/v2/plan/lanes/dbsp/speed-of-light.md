---
status: plan
summary: Lane dbsp's speed of light: the bound of each maintained view per edit and cold, the arithmetic, and how the design comes within 10% of it.
date: 2026-10-30
---
# dbsp: speed of light

Machine as in [../db/speed-of-light.md](../db/speed-of-light.md): about 3.5 GHz, L1 in 4 to 5
cycles, L2 in 14, L3 in 50, DRAM in 110 ns, and about 2 instructions per cycle on branchy
code. Design: [../../design/dbsp.md](../../design/dbsp.md).

## Per edit

The floor is the work an oracle that knows the changed facts would do. It has to read the
edited module's new facts, change the derived facts that depend on them, and tell each reader of
a changed slice.

| step | bound | design | over the bound |
|---|---|---|---|
| the edited module's log vs the old one | one pass over its ops: `n` ops × (hash 32 B + compare) ≈ `n` × 2 ns | prefix and suffix compare, then a multiset diff of the middle by FxHash. A 200-item module takes about 0.4 µs | none: it is the pass |
| applying `d` op deltas | `d` × (one probe of the candidate map + one aggregate) ≈ `d` × 30 ns (two L2/L3 misses) | the collector's `update` on an FxHash `(ns, name)` map, plus a read-index probe per changed binding | the `update` code's own branches; measure |
| waking `r` readers | `r` × (re-resolve one path: `s` segments × one probe) ≈ `r·s` × 20 ns | the read index gives readers with no scan; each re-resolves with `path_resolution` over the state | none |
| DRed region of size `R` (deletes only) | `R` × (rebuild its candidates) | over-delete through the read index, then rebuild each region module's scope from its Z-set | Backward/Forward would skip the over-delete of still-supported facts. Worth it if `R` is often larger than the readers that change; measure |
| publishing `p` slices | `p` × one salsa input set (about 100 ns) + BLAKE3 of the slice (about 1 GB/s on one core for small inputs, so 64 ns per 64 B row) | one set per changed slice, hash over its rows | none |

**Adding one impl.** About 0.4 µs to diff the 200-op module, plus one op, one `ImplKey`
(header lowering: the impl's own work), one row, one slice set and one hash. That is under 2 µs
of maintainer time beyond the item tree and header lowering. Today's pull path verifies
`O(k·m)` round memos, reassembles the `DefMap` (`O(m)`) and reruns `modules_by_trait` (`O(m)`).
For `m = 1000` and `k = 4`, that is about 4000 × 50 ns + 2 × 1000 × 30 ns ≈ 260 µs.

**Adding `fn foo` to a module with 3 glob importers.** The log diff, one candidate, one binding,
3 glob candidates downstream (each one aggregate) and their readers of `foo`. That is usually 0
readers, so 4 scope slices are set. Bound: about 1 µs.

### Impls

The impl maintainer is described in [impls.md](impls.md). It runs on name resolution's delta,
so the floor is the impls and traits that delta names, plus the impls whose read set it meets.

| step | bound | design | over the bound |
|---|---|---|---|
| keying `d` added impls | `d` × (header lowering, which the impl must do anyway) | `impl_key` over the lowered header, plus one row insert per arrangement | none |
| waking `r` impls on changed bindings or defs | `r` probes of the mentions index ≈ `r` × 20 ns, plus their rekeys | FxHash maps from `(module, name)` and def to impls | none |
| publishing `p` slices of `n` rows | `p` × (one input set ≈ 100 ns) + `n` × 64 ns of BLAKE3 | one set per changed `(crate, trait)`, `(crate, self type)` or `(crate, name)` slice; the directory only when a key appears or goes | a slice is rehashed whole: `O(n)`, not `O(1)`. A trait with thousands of impls in one crate pays microseconds; measure |
| a warm read | one memo probe, no allocation | the reader's memo over the slice input | none |
| re-reading body parts | the modules a write touched: `t` memo checks | `body_part` of the touched modules, the modules of changed bindings, and the `h` modules with impl holders | `h` checks per write that changed anything, plus one check of `macros_writing_impls`; a change of which macros write impls reads all `m` |
| a read after an edit | one slice read + the `b` modules' body tables that the slice lists | slice rows merged with those tables by `(module rank, part, position)` | none: no reader goes over the crate's modules |

**Adding one impl** to a 1000-module crate gives
`Work { ops: 1, woken: 1, rebuilt: 1, keys: 1, slices: 1 }`: one key, one row, one slice set
and one re-read of the leaf's body part. It is the same at 10 modules as at 1000. **Editing a
body** gives `Work { woken: 1 }` and reruns no reader. The test is
`delta/tests/cost.rs`.

## Cold

The cold run is the delta from empty, so its cost is the collector's (one pass over the items
plus the import fixpoint) plus the indexes:
- one read-index entry per path segment read, about 16 B;
- one candidate per binding source.

That is about 1.3× the facts resolver's memory for the resolution part. The indexes are flat
FxHash maps with `u32` handles. Their rows come from `blaze-mem` pools, and nothing is
allocated per probe.

## State of the art

- Faster than rustc, which reruns resolution whole: nothing incremental before its query
  system starts.
- Faster than rust-analyzer, whose `crate_def_map` runs again per crate on any item change.
- Faster than `facts/`, which is pull-based with `O(k·m)` verification.
- Matches Hack's incremental server: per-file facts, a reverse dependency index, re-checking
  only the readers of changed names.
- Matches differential dataflow's arrangement updates, at `O(Δ)` per operator.
- Does not beat DBSP's nested-fixpoint cost for recursion, which is about as fine as DRed on
  this workload. Backward/Forward is the next step if regions measure large.
