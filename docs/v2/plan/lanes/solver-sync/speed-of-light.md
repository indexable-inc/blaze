---
status: plan
summary: speed of light for solver-sync's components (the type, region and constant tables and their handles, generic-argument packing, the persisted codec, goal evaluation in the vendored solver, and tools/v2/solver_sync.py), with the arithmetic, what the code costs today and what removes each gap.
date: 2026-10-30
---
# Speed of light: solver-sync's components

The bar is "Within 10% of the theoretical maximum, everywhere" in
[principles.md](../../goals/principles.md). solver-sync owns `vendor/rustc_type_ir`,
`vendor/rustc_next_trait_solver`, `vendor/rustc_type_ir_macros`, `tools/v2/solver_sync.py`
and 18 files of `crates/hir-ty/src/next_solver/`: `interner.rs` with its `mod table`, `ty.rs`,
`region.rs`, `consts.rs`, `generic_arg.rs`, `predicate.rs`, `persist.rs` and the rest. Lane db
covers the solver glue, the solver cache and type relations
([speed-of-light-solver-glue.md](../db/speed-of-light-solver-glue.md),
[speed-of-light-solver-cache.md](../db/speed-of-light-solver-cache.md),
[speed-of-light-infer-relate.md](../db/speed-of-light-infer-relate.md)).

Machine figures are lane db's ([speed-of-light.md](../db/speed-of-light.md)): 3.5 GHz, an L1
hit in about 4.5 cycles, DRAM about 110 ns, about 20 misses in flight per core, one core
streaming about 30 GB/s (the shared table is
[link/speed-of-light.md](../link/speed-of-light.md)'s "The machine"; its 3.5 GHz and DRAM
figures agree). Nothing is measured. Costs are counted from the code as of
2026-10-29 20:20, and `proof`'s budgets measure them once the tree builds. A "line" is a
dependent load. "Hot" means every line is in L1.

## 1. Interning a value that is already there (`table::Rows::intern`, `Lists`, `Values`)

This is the most frequent table operation: every `Ty::new`, every fold that rebuilds a type,
and every persisted value read back.

- **Work:**
  - hash the kind (a `TyKind<'static>` is 16 to 24 bytes);
  - find the slot holding its row;
  - compare the row's kind with the value.
- **Bound:**
  - 2 lines that can't be avoided: the slot, then the kind it names.
  - About 10 cycles to hash 3 words.
  - About 5 cycles of compare and branch.
  - Hot: 2 × 4.5 + 15 ≈ **24 cycles, 7 ns**.
  - Cold: 2 dependent misses, 220 ns of latency. Independent interns overlap, so the
    throughput cost is about 11 ns.
- **Today, after this change:** 4 dependent lines, about 41 cycles hot, **1.7× off**. The
  lines are:
  1. the shard's pointer;
  2. the slot;
  3. the kind column's segment base;
  4. the kind.

  The hash is Fx plus murmur's finalizer, about 14 cycles. Cold, it is at the bound: the shard
  array (64 × 16 bytes) and the segment bases stay in L1, so only the slot and the kind miss.
- **Changed now:** the shard's pointer used to point at a `Slots { words: Box<[AtomicU64]> }`,
  so a probe read the box's header before the slot: 5 lines, about 46 cycles hot.
  - The shard now keeps the table's address with the log2 of its length in the low 6 bits.
    The table is a 64-byte-aligned allocation; `map_addr` keeps its provenance.
  - Tests: `a_shard_pointer_gives_back_its_table_and_length` and
    `a_shard_that_grows_keeps_every_row`, beside the table's other tests.
- **What stands in the way, and what removes it:**
  1. **The segment base (line 3).** Columns grow in segments so that rows never move.
     - Reserving each column's whole range of virtual addresses once (2^30 rows × the row's
       size) and committing pages as rows arrive gives the same never-moving rows. A row's
       address is then `base + row × size`, with `base` a constant of the `static`.
     - That removes line 3 and the 4 instructions of `locate`: about 24 + 9 ≈ 33 cycles,
       1.4× off.
     - It needs the reservation in hir-ty (blaze-mem's regions do this for salsa's memos), and
       hir-ty now depends on blaze-mem (10-30, `crates/hir-ty/Cargo.toml`).
     - Two things remain open, so this is not applied (sol-all-ty, 10-30). The column
       statics are `const`, so a `Region` can't be their base. The base becomes one lazily
       set `AtomicPtr` read relaxed, about 1 L1 hit, which is a smaller win than the 33 cycles
       above: about 4 to 9 cycles, the `locate` chain. And reserving 2^30 rows for every
       column is about 8 GB of address space per 8-byte column, a few hundred GB in all,
       committed by touch. Whether that fits every host and sandbox (`RLIMIT_AS`, macOS) is a
       decision for Andrew, and the saving needs a measurement.
  2. **The second mix of the hash.** Fx's high bits are well mixed but its low bits are not, so
     the finalizer is there for the slot index, which uses the low half. Taking the shard from
     bits 58 to 63 and the slot from bits 26 to 57 of the plain Fx hash drops the finalizer,
     about 8 cycles: 24 + 2 ≈ 26 cycles, **within 10%**. Before that lands, the slot
     distribution has to be checked on the corpus's real types, since a bad spread makes the
     probe sequences longer.
  3. **The shard's pointer (line 1)** is an L1 hit, and a dependent one. It goes away only if
     the index is a single table, and a single table needs a lock-free grow. It stays: about
     4.5 cycles, inside the 10%.

## 2. Reading a handle (`Ty::kind`, `flags`, `outer_exclusive_binder`)

The solver and every fold read these many times per interned value.

- **Work:** one load of the field from the row.
- **Bound:** 1 line plus a multiply-add for the address: hot about **6 cycles**, cold one miss.
- **Today:** two independent chains, each a segment base and then a value.
  - The first chain loads the row's tag byte and checks that the row was published.
  - The second chain loads the field itself.
  - The 2 chains run in parallel, plus about 10 instructions (`locate` twice, the compare):
    about 15 cycles hot, **2.5× off**.
  - Cold, it is close to the bound: the tag column is 1 byte a row, so it is mostly in cache.
- **What stands in the way, and what removes it:**
  1. **The per-read check.** `kind()` has to check the tag because `at(row)`, which makes a
     handle from a bare row, is a safe `pub(crate)` function. Any code could make a handle to
     a row that isn't written yet, and only the check keeps `kind()` sound.
     - The fix: `at` becomes `unsafe` ("this row was returned by this table, or passed
       `published`"). Its callers each state why their row qualifies: the unpacking of a
       packed `GenericArg`, `persist.rs`'s reader, and the region and constant handles.
       `from_index` already checks `published`.
     - Then `kind()` and `flags()` read the field with no check. The acquire load that made
       the handle (an index probe or `published`) orders the row's writes before the read, as
       it does today.
     - Owner: solver-sync, in its own files. It is next, because the audit of every `at`
       caller has to be done in one change.
  2. **The segment base:** as in section 1, item 1. With both fixes the read is 1 line and a
     multiply-add, at the bound.

## 3. Packing generic arguments (`generic_arg.rs`)

A `GenericArg` is a `u32`: the row shifted left by 2, plus a 2-bit tag. Packing is a shift and
an or; unpacking is a shift, a mask and a match on the tag. Both take about 2 cycles and touch
no memory: **at the bound**. A list of arguments is a `Lists` row (section 1): its items are
contiguous `u32`s, 4 bytes each, so walking a list of 4 reads one line.

## 4. Flag tests in folds and visits (`has_type_flags`, `has_escaping_bound_vars`)

Every fold and visit that may return early does one of these per node, for example
`has_non_rigid_aliases` in normalization and `HAS_CANONICAL_BOUND` in instantiation.

- **Bound:** the flags field, 1 line plus an `and`: about 6 cycles hot.
- **Today:** the same reads as section 2, about 15 cycles hot, 2.5× off. The same two changes
  reach the bound.
- The vendored solver's folders already return at the first node whose flags rule them out,
  as rustc does. That part is at the bound once the flag read is.

## 5. The persisted codec (`persist.rs`)

A query result keeps its types in a table of its own: each value is written once, after its
children, as `u32` words.

- **Writing:** per value, one probe of the writer's `FxHashMap` (about 30 cycles) plus its
  words. A value is 3 to 6 words, so a table of 10^4 values is about 200 KB, written in about
  7 µs at 30 GB/s. The map dominates (10^4 × 30 cycles ≈ 86 µs).
  - The bound is the same probe: the writer has to find out whether it already wrote the
    value, which takes one probe either way.
  - That probe can move to the global table instead: a value's place in the persisted table
    could be a column indexed by its global row and reset per table. That is one line instead
    of a hash and a probe, about 10 cycles instead of 30. It is worth doing only once the
    budgets show writing on a hot path; a warm build writes only results that changed.
- **Reading:** each value is interned into the process's table again. That intern is
  unavoidable, since equal values must be one row in a process, and it usually hits (section
  1). The words are read once, front to back, so the read is memory-bound with nothing to
  shave beyond section 1's intern: at section 1's cost, so **within section 1's gap**.
- The debug-build round-trip check doesn't run in release.

## 6. Evaluating a goal in the vendored solver (`vendor/rustc_next_trait_solver`)

The most common goal in a warm session is one whose answer is in the global cache.

- **Work:**
  - canonicalize the goal: with no inference variables, one flag test answers it;
  - intern the canonical input: an intern hit, section 1;
  - look the answer up in the global cache: lane db's solver-cache doc puts the lookup at a
    few lines;
  - instantiate the response: nothing to do when it has no variables.
- **Bound:** about 24 + 30 + 20 cycles plus the calls between them: **about 100 cycles, 30 ns**.
- **Today:** not counted yet. The vendored solver is rustc's at nightly-2026-10-27. A root
  goal goes through `evaluate_root_goal`:
  - it builds an `EvalCtxt` and a search-graph frame;
  - it makes the proof-tree builder's no-op checks;
  - it instantiates the canonical response, empty or not.
  rustc's own profiles put a cached next-solver goal at several hundred nanoseconds, which
  would be **about 10× off**. This is the biggest gap in solver-sync's components.
- **What stands in the way, and what removes it:**
  1. Nothing builds yet, so the cost can't be measured, and patching the solver's hot path
     without a measurement would be a guess. Once the tree builds, `proof`'s budget for "a
     cached root goal" measures it.
  2. Then, a patch in the series (`vendor/rustc_next_trait_solver/PATCHES.md`): on a root goal
     with no inference variables, look the canonical input up in the global cache before
     building the `EvalCtxt`, and return the cached response directly when it has no
     variables to instantiate.
     - rustc's own fast path (`SolverDelegate::compute_goal_fast_path`, which hir-ty
       implements) already does this for goals that need no cache: `Sized` of a sized type,
       outlives goals between constants.
     - The patch is a blaze change kept through each nightly by `solver_sync.py`. It should go
       upstream too: rustc gains the same.
  3. hir-ty's fast path, the delegate's `compute_goal_fast_path` in `solver.rs`, is lane db's
     (db-ty-port-solver). It answers a goal before the solver is entered, with no allocation.

## 7. `tools/v2/solver_sync.py`

This runs once per nightly bump, off every build's path.

- **Work:** fetch three directories at one commit (about 3 MB of sources), copy them, and apply
  about 10 patches.
- **Bound:** 3 MB at 100 MB/s is about 30 ms, plus 2 round trips to GitHub (about 100 ms);
  copying and `git apply` take about 50 ms. So **about 0.2 s**.
- **Today:** the clone is shallow (`--depth 1`) with `--filter=blob:none` and a sparse
  checkout. The fetch brings every tree of that commit, about 10 MB for rust-lang/rust, then
  one batch of blobs for the three directories. About 0.5 to 1 s, **3 to 5× off**.
- **What stands in the way:** fetching only the trees on the three paths takes
  `--filter=tree:0`. git then fetches each tree it needs lazily, one round trip each, which
  is slower over a network than taking all trees at once. 1 s once a day is not worth the
  complexity, so the script stays as it is. Named here, as the bar asks.

### Correction (speed-sol2-small, 2026-10-30)

The 0.5 to 1 s above is the first run only. `fetch` keeps its clone under the cache directory
(`solver_sync.py` `fetch`, a `git init` only when there is none), so a daily bump fetches one
new commit against yesterday's: git's negotiation sends yesterday's commit as a have, and the
pack holds only the trees and blobs that changed under the sparse paths, plus one round trip for
the checkout's missing blobs. That is the bound's 2 to 3 round trips and a pack of tens to
hundreds of KB: **within 10% of the 0.2 s bound on a daily bump**, off it only on a cold cache,
where the protocol (no path-filtered tree fetch) sets the floor.

## Where each gap stands (speed-sol2-small, 2026-10-30)

solver-sync is done, but every file above except `tools/v2/solver_sync.py` is also lane db's,
and db is not done, so the changes are queued to db with the exact edit:

| section | today | bound | gap | queue |
|---|---|---|---|---|
| 1. an intern hit | ~41 cycles | ~24 | 1.7× | 00149 (reserved column bases), 00887 (one mix of the hash) |
| 2. a handle read | ~15 cycles | ~6 | 2.5× | 00886 (`at` unsafe, no per-read check), then 00149 |
| 3. packing | ~2 cycles | ~2 | at the bound | none |
| 4. a flag test | ~15 cycles | ~6 | 2.5× | 00886 |
| 5. the persisted writer | ~30 cycles a value | ~10 | 3× | 00889 (a column by global row) |
| 6. a cached root goal | several hundred ns (rustc's profiles) | ~30 ns | ~10× | 00888 (vendor patch: global cache before `EvalCtxt`) |
| 7. `solver_sync.py` | 2 to 3 round trips daily | 0.2 s | within 10% daily | none |

## What `proof` should measure (speed-sol2-small)

- Section 1: per intern hit, cycles (target 26) and allocations (0), over the corpus's types
  replayed into a fresh table, and the mean probe length (the check 00887 needs).
- Sections 2 and 4: cycles per `Ty::kind` and per `has_type_flags` over a fold of a large type
  (target 6 and 7 hot), and zero allocations.
- Section 5: per value written, cycles (target 10) and bytes written per value, on the table of
  a large inference result; per value read, one intern hit.
- Section 6: time per cached root goal (target 30 ns) and allocations (0), the goal mix of a warm
  `blaze check` of a large crate replayed.
- Section 7: wall time and bytes received per daily bump, from git's trace (`GIT_TRACE_PACKET`).

## State of the art

- **Tables:** compared with rustc's interner (a sharded `FxHashMap` of arena pointers,
  dereferenced directly) and with the best lock-free interners (open-addressed, read without
  locks). Today a hit matches them in lines touched when cold. A handle read is behind rustc's
  pointer dereference (section 2) until the per-read check and the segment base are gone.
- **Handles:** 4 bytes against rustc's 8-byte pointers, so a `GenericArgs` list touches half
  the lines.
- **Solver:** rustc's next solver is the state of the art, and blaze runs it unchanged except
  for the patch series. Section 6's fast path would go beyond it.
