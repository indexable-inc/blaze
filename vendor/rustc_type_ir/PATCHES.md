# Patches on rustc's `compiler/rustc_type_ir`

Base: rust-lang/rust `16d1ffac17ad8b739e32483ac2e65fe7e1804977`, nightly-2026-10-27.

`src/` is upstream's `compiler/rustc_type_ir/src` at the base commit with the series below
applied in order (`docs/v2/plan/design/compile-time.md`, change 18). `tools/v2/solver_sync.py`
replaces the base on every toolchain bump and replays the series; it stops at the first patch
that no longer applies, naming it. Each `### NNNN-name` heading is `patches/NNNN-name.patch`, a
diff of `src/` taken with `a/` and `b/` prefixes. To change the series: edit `src/`, regenerate
the patch, and keep this file's headings and the `patches/` directory one for one.

`Cargo.toml` is blaze's, built as rust-analyzer's `ra-ap-rustc_type_ir` builds the crate:
`nightly` off, so its dependencies on the compiler's own crates are left out. `Cargo.toml.orig`
is upstream's manifest at the base. The script stops if upstream's changes, until
`Cargo.toml` follows it.

## Series

### 0001-from-checked-parts

What: `TraitRef`, `ExistentialTraitRef`, `ExistentialProjection` and `Alias<I, K>` (so every
`AliasTy`, `AliasTerm`, `AliasConst`, ...) gain `from_checked_parts`, which builds the value
from its parts with no interner and no `debug_assert_*_args_compatible`.

Why: hir-ty's `next_solver/persist.rs` rebuilds these from interned rows and persisted type
tables, where no `DbInterner` exists. The parts were checked when the value was first made.
The fields are private (`_use_*_new_instead`), so only this crate can build them.

### 0002-opaque-of-impl

What: `Interner::opaque_of_impl(param_env, opaque, args) -> Option<Ty>`, `None` by default.
`rustc_next_trait_solver`'s patch `0001-opaque-of-impl` calls it when revealing an opaque type.

Why: hir-ty models a trait method's return-position `impl Trait` as an opaque type of the trait.
Once its arguments name an impl, its hidden type is the impl method's opaque type, which
`type_of` of the trait's opaque can't say. rustc lowers the same thing to a projection, so it
needs no hook.

### 0003-shared-solver-cache

What: `search_graph::GlobalCache` is rewritten for sharing across threads, and the cache API
gains a scope:
- Goals and distinct results are interned as `u32`s in chunked tables that never move, with
  per-shard open-addressed levels.
- Rows are published once and never changed. A lookup is lock-free acquire loads that allocate
  nothing, and an insert locks one stripe by goal.
- `Cx::with_global_cache(scope: CacheScope, f: FnOnce(&GlobalCache) -> R)` (it was `&mut`) and
  `Cx::cache_scope(&DepNodeIndex) -> CacheScope`, with `CacheScope { Shared, Query }`, in
  `search_graph` and on `Interner`. A lookup tries `Shared`, then `Query`. An insert goes where
  `cache_scope` puts the goal's dep node.
- rustc's two asserts are kept, comparing results by value. The file has its own tests.

Why: blaze's database is shared by `&` across worker threads
(`docs/v2/plan/design/concurrency.md`), and solved goals are reused across queries
(compile-time.md change 17). rustc's cache is one `HashMap` behind one lock for the session.
Lane db's notes: `docs/v2/plan/lanes/db/trait-solver.md`.

### 0004-unsafe-code-policy

What: `#![deny(unsafe_code)]` on the crate, with `allow` only where upstream needs `unsafe`, each
with its `// SAFETY:` argument:
- `fold::fold_arc`, which folds a unique `Arc` in place, with tests against the safe
  `Arc::new(fold(Arc::unwrap_or_clone(arc))?)`;
- the `unsafe impl GenericTypeVisitable` impls of `generic_visit`;
- the one emitted by `TrivialTypeTraversalImpls!`.

Why: `docs/v2/plan/design/unsafe.md`. `rustc_type_ir_macros`' patch `0001` lets the derive's
own `unsafe impl` through the `deny`.

### 0005-simplified-type-ord

What: `SimplifiedType` derives `PartialOrd` and `Ord` (every field already is `Ord`).

Why: hir-ty keeps impls keyed by simplified self type in sorted tables (`v2/requests/items.md`,
"items-hirty: simplified self types as `u32`"). The order is only a table key: no output may
depend on it, since it follows `DefId`'s order.

### 0006-solver-storage-recycled

What: the search graph's per-goal temporaries live inline or in storage a worker keeps:
- `CycleHeads` and `CandidateHeadUsages` are `SmallVec`s of `(StackDepth, _)` sorted by depth
  (4 and 2 inline) instead of `BTreeMap`s; every lookup, insert and iteration keeps upstream's
  depth order.
- `SearchGraphStorage<X, S>` holds an emptied stack, provisional cache, the provisional cache's
  spare entry lists and the delegate's `Delegate::Scratch`. `SearchGraph::new_in` builds a graph
  from one, `into_storage` empties the graph back into one, keeping capacity; `new` is
  `new_in` with empty storage. Entry lists the provisional cache drops go to the spares.
- `Interner::mk_external_constraints_ref(&data)`, by default `mk_external_constraints` of a
  clone, so an interner can intern from a borrow and the caller reuse the buffers.
- `InferCtxtLike::clone_opaque_types_added_since_into(prev, &mut Vec)`, by default extending
  from `clone_opaque_types_added_since`, so the caller's list can be a reused one.
- Tests (with `blaze-alloc-count` as the test allocator): three cycle heads keep depth order
  without allocating, merging two-head usages allocates nothing, a recycled graph holds what it
  held and starts empty, and a recycled graph solves an acyclic goal without allocating.

Why: `v2/requests/mem-db-solver.md` sections 1, 2 and 4. Every root goal built a fresh graph and
`BTreeMap` nodes per cycle head; type inference solves millions of goals. The proof-tree entry
(`evaluate_root_goal_for_proof_tree_raw`) still uses `new`: no delegate reaches it, and it is a
diagnostics path.

### 0007-fast-path-depth

What: `solve::FastPathDepth` (`Added`, `Evaluating(Option<usize>)`, `Any(Option<usize>)`, with
`depth_free` and `available`), which says what `SolverDelegate::compute_goal_fast_path` may
answer where it is asked, and `ComputeGoalFastPathOutcome::HoldsWithin { required_depth }`.
`SearchGraph::nested_available_depth(lower_available_depth)` gives the depth a nested goal
evaluated now gets (`evaluate_goal`'s `allowed_depth_for_nested`), and
`SearchGraph::charge_answered_nested(available, required)` charges the current goal as a
global cache hit of that required depth with no nested goals is charged.
`rustc_next_trait_solver`'s patch `0005-fast-path-depth` uses them.

Why: hir-ty's shape table proves auto-trait and one-impl `Copy`/`Clone` goals that rustc
proves through the search graph. Answered without charging depth, a goal near the recursion
limit held in blaze where rustc overflows (E0275). An answer that stands for an evaluation now
fits in, and costs, the depth the search graph would give it
(`docs/v2/plan/lanes/frontend/fast-trait.md`, "Depth"; `v2/requests/db-ty.md`, "the shape table
charges search-graph depth as rustc does").

### 0008-def-id-is-local

What: `inherent::DefId::is_local(self, cx: I) -> bool`: it takes the interner.
`rustc_next_trait_solver`'s patch `0006-def-id-is-local` passes it at coherence's two calls.

Why: rustc compares an id's crate with `LOCAL_CRATE`, a constant of the session. blaze's ids
carry their crate, but one database checks many crates, so the crate being checked is the
interner's (`DbInterner::krate`) and an id alone can't say whether it is local. Without it every
trait ref counted as local, and coherence (`TypingMode::Coherence`) never saw an upstream or
downstream crate's possible impl.

### 0009-small-sets-and-stalled-lists-inline

What:
- Without `nightly`, `data_structures::SsoHashSet` is `data_structures/sso.rs`'s set: up to 8
  elements inline, searched linearly, then an `FxHashSet`, as `rustc_data_structures::sso` is.
  It was `std::collections::HashSet`. `SsoHashMap` is unchanged (nothing uses it).
- `universe::MaxUniverse`'s cache is an `SsoHashSet`, not a `HashSet`.
- `solve::GoalStalledOn`'s `stalled_vars` and `sub_roots` are `StalledVars`
  (`SmallVec<[TyOrConstInferVar; 2]>`) and `SubRoots` (`SmallVec<[TyVid; 2]>`), not `ThinVec`s.
  `rustc_next_trait_solver`'s patch `0007-solver-temporaries-kept` builds them.

Why: blaze holds the solver's warm paths to zero allocations (`docs/v2/plan/lanes/mem/db-solver.md`,
items 8 and 10). A `HashSet` allocates on its first insert, so `ValidateBoundVars` (every
`Binder::bind_with_vars` and `map_bound` with debug assertions), `TypeWalker`, the outlives
collector and `MaxUniverse` each allocated where rustc's own build, with `nightly`, keeps the
set inline. A stalled goal waits on one or two variables almost always, and its `ThinVec`s
(rust-lang/rust#160005, for a small `GoalEvaluation`) allocated twice per stalled goal and per
probe clone. The inline lists cost 24 to 40 bytes more per nested goal.

### 0010-solver-cache-in-blocks

What: `search_graph::global_cache` (patch `0003-shared-solver-cache`) keeps its chunks, shards,
writers and probe tables in `blaze_mem::blocks::Slice`s (power-of-two blocks of OS memory, given
back to their size class when the cache drops) instead of `Box<[T]>`s, and its shard and writer
locks are `blaze_mem::lock::SpinLock`s instead of `std::sync::Mutex`es. `Cargo.toml` (blaze's)
depends on `blaze-mem` for it.

Why: blaze makes a cache per interner, and drops it when the interner's generation is evicted
(`hir-ty`'s `next_solver::interner::tls_cache`). In `Box`es a fresh cache made about two dozen
heap allocations the first time it was used, so re-solving an unchanged body allocated
(`docs/v2/plan/lanes/mem/db-solver.md`, items 8 and 10). On macOS a `std` mutex boxes a pthread
mutex the first time it is locked, which cost one more allocation per shard and writer used.
Blocks come from reserved regions, so a fresh cache's tables cost the global allocator nothing,
and the blocks of an evicted cache are the next cache's, already backed by pages. Lookups and
inserts are unchanged; an insert's critical section is a few instructions, so a spin lock that
yields is enough.

## Dropped since the previous base (ra-ap-rustc_type_ir 0.166.0, rustc `ce89c898`)

- The backport of rust-lang/rust#159228 (the retry with twice the recursion limit:
  `InferCtxtLike::commit_if_ok`, `Certainty::is_overflow`, `root_depth` on
  `evaluate_root_goal_for_proof_tree_raw`): upstream has it.
- `fmt` (f9a83eaf47): formatting of the old base only.
