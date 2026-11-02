# Patches on rustc's `compiler/rustc_next_trait_solver`

Base: rust-lang/rust `16d1ffac17ad8b739e32483ac2e65fe7e1804977`, nightly-2026-10-27.

`src/` is upstream's `compiler/rustc_next_trait_solver/src` at the base commit with the series
below applied in order (`docs/v2/plan/design/compile-time.md`, change 18).
`tools/v2/solver_sync.py` replaces the base on every toolchain bump and replays the series; it
stops at the first patch that no longer applies, naming it. Each `### NNNN-name` heading is
`patches/NNNN-name.patch`, a diff of `src/` taken with `a/` and `b/` prefixes. To change the
series: edit `src/`, regenerate the patch, and keep this file's headings and the `patches/`
directory one for one.

`Cargo.toml` is blaze's, built as rust-analyzer's `ra-ap-rustc_next_trait_solver` builds the
crate: `nightly` off. `Cargo.toml.orig` is upstream's manifest at the base. The script stops if
upstream's changes, until `Cargo.toml` follows it.

## Series

### 0001-opaque-of-impl

What: revealing an opaque type in `Reflection`, `PostAnalysis` and `Codegen`
(`project_goals/opaque_types.rs`) first asks `Interner::opaque_of_impl` (`rustc_type_ir` patch
`0002`). It falls back to `type_of` of the opaque only when the hook says `None`, then normalizes
either.

Why: see that patch. hir-ty's trait-method opaque types, once their arguments name an impl,
reveal to the impl method's opaque type.

### 0002-unsafe-code-policy

What: `#![deny(unsafe_code)]` on the crate.

Why: `docs/v2/plan/design/unsafe.md`. The crate has no `unsafe` of its own. It is `deny`, not
`forbid`, because `TrivialTypeTraversalImpls!` (in `coherence`) expands to `rustc_type_ir`'s
allowed `unsafe impl GenericTypeVisitable`, and a `forbid` would refuse that `allow`.

### 0003-rustc-rules-at-unimplemented-sites

What: each `unimplemented!` upstream leaves on a path hir-ty can reach is replaced:
- `canonicalizer`, `InferConst::Fresh`: the same panic as the type arm; fresh variables are
  never canonicalized.
- `effect_goals`: `[const] FnPtr` and `[const] AsyncFn*` builtin candidates are `NoSolution`.
  No const impl of them exists, as upstream already answers for `[const] Iterator`.
- `solve::compute_const_arg_has_type_goal`:
  - a non-rigid `ConstKind::Alias` after `structurally_normalize_const` is `unreachable!`,
    because the solver never binds an inference variable to a non-rigid alias;
  - `ConstKind::Expr` is `unreachable!`, because the interner's `generic_const_exprs` is off
    and nothing lowers one.
- `normalizes_to`, `<unsafe<..> T as Pointee>::Metadata`: it is `T`'s metadata when `T` names
  no lifetime of the binder. Otherwise it is `()` given `for<..> T: Sized`, since an unsafe
  binder is as sized as the type it binds.
- `normalizes_to`, the discriminant of an unsafe binder: `discriminant_ty`, as for any other
  type without variants.

Why: blaze stops with an error, never a panic stub, on input rustc accepts
(`docs/v2/plan/goals/principles.md`). Each answer is the one rustc's own rules give, or an
`unreachable!` with the invariant that rules the case out.

### 0004-solver-temporaries-recycled

What: the eval context's temporaries come from `SolverScratch` (`solve/scratch.rs`), pools of
emptied buffers kept in the search graph (`rustc_type_ir` patch `0006`) as its
`Delegate::Scratch`. A buffer is taken, used and given back on every path out; one that never
grew is not kept.
- `SolverDelegate::obtain_search_storage` and `release_search_storage` (by default a fresh one,
  and drop): `EvalCtxt::enter_root` builds its graph from the delegate's storage and returns it.
- Pooled: nested goal lists (`enter_root`, `enter_canonical`, probes, each step of
  `evaluate_added_goals`), candidate lists (assembly, `merge_trait_candidates`' picks), the
  universes of `characterize_param_env_assumption`, the original values of `canonicalize_goal`,
  the option list of the query response's instantiation, the outlives dedupe set, the region
  variable set of `filter_irrelevant_region_constraints`, and the region constraints, opaque
  types and normalization goals of external constraints, interned by
  `mk_external_constraints_ref`.
- `canonicalize_goal` and `canonicalize_input` fill a given `Vec`; `finalize` no longer takes
  the canonicalizer's variables. `instantiate_and_apply_query_response` takes the option list.
  `build_stalled_on` takes a slice.
- `SolverDelegate::make_deduplicated_region_constraints_into(&mut Vec)`, by default extending
  from `make_deduplicated_region_constraints`.
- Upstream's comment in `evaluate_added_goals_step` on why the goal list is taken each step is
  replaced by one on the spare list it now swaps in.
- Tests (with `blaze-alloc-count`): a pool keeps capacity, drops a buffer that never grew, and a
  warm take, push and give allocates nothing.

Why: `v2/requests/mem-db-solver.md` sections 3 and 4. Each goal allocated these lists fresh;
type inference solves millions of goals.

### 0005-fast-path-depth

What: `SolverDelegate::compute_goal_fast_path` takes a `FastPathDepth` (`rustc_type_ir` patch
`0007-fast-path-depth`), and `fast_path::compute_goal_fast_path{,_cold}` take it too and return
the levels a `HoldsWithin` answer needs beside the evaluation, asserting each answer is one the
depth allows. `add_goal` asks with `Added` (only answers that cost no depth). The nested
`evaluate_goal` asks with the depth the search graph gives the goal (`Evaluating`, or `Any` for
a stalled goal's rerun; none while a proof tree is built) and charges the parent with
`charge_answered_nested`. The root's stalled rerun asks with `Any` at the recursion limit.
`evaluate_in_search_graph`'s choice of `LowerAvailableDepth` moves, unchanged, into
`EvalCtxt::lower_available_depth`, which both use.

Why: as `rustc_type_ir`'s `0007-fast-path-depth`: an answer that stands for a search-graph
evaluation fits in, and costs, the depth it would be evaluated with, asked where the goal is
evaluated, not added, since a sibling that overflows in between divides that depth.

### 0006-def-id-is-local

What: coherence's `trait_ref_is_local_or_fundamental` and `OrphanChecker::def_id_is_local` pass
the interner to `DefId::is_local` (`rustc_type_ir` patch `0008-def-id-is-local`): `tcx`, and
`self.infcx.cx()`.

Why: see that patch.

### 0007-solver-temporaries-kept

What:
- `EvalCtxt::build_stalled_on` builds `SubRoots` (`rustc_type_ir` patch
  `0009-small-sets-and-stalled-lists-inline`), not a `ThinVec`.
- On a canonical param-env cache hit, the canonicalizer refills its recycled `variables` with
  `clear` and `extend_from_slice`. `ThinVec::clone_from` is the default one, a new clone,
  which dropped the recycled capacity.
- `BoundVarReplacer::replace_bound_vars` copies `universe_indices` only when
  `assumptions_on_binders` is on, the only case that reads the copy.

Why: each allocated once per goal on warm storage (`docs/v2/plan/lanes/mem/db-solver.md`,
item 8). Answers are unchanged.

### 0008-scratch-buffers-raised

What: `solve::scratch` (patch `0004-solver-temporaries-recycled`) grows a buffer given back
to the largest capacity its pool holds.

Why: the pools hand buffers out last in, first out, so a goal could get a buffer smaller than
one a sibling had outgrown and grow it again on the next run; warm re-solves of one body still
allocated a nested-goals list (`docs/v2/plan/lanes/mem/db-solver.md`, item 8). Raised, every
buffer a warm run takes holds what any goal of the run needed. A pool now holds its depth times
its largest buffer.

### 0009-bailed-probes-give-back-their-goals

What: `ProbeCtxt::enter_inner` gives its nested-goals list back to the scratch pool (patch
`0004-solver-temporaries-recycled`) when it bails because its context must rerun outside erased
mode, instead of dropping it.

Why: each bail cost the pool one warm list, so a later probe of the same run found the pool
empty and allocated one; a warm re-solve of an unchanged body allocated once per run
(`docs/v2/plan/lanes/mem/db-solver.md`, item 8). Answers are unchanged: nothing reads the list
after a bail.

### 0010-never-grown-scratch-buffers-kept

What: `solve::scratch` (patches `0004-solver-temporaries-recycled` and
`0008-scratch-buffers-raised`) keeps a buffer given back that never grew, raised to the largest
its pool holds, once the pool holds any room; before, only buffers that had grown were kept.

Why: `evaluate_added_goals_step` swaps the list it evaluates for a spare from the pool, so a
grown list goes back to the pool at a shallower level than the one that took it, and the spare
that took its place, never grown, was dropped. Taken last in, first out, the one grown list went
to an outer goal on the next run and the probe that grows a list found the pool empty again,
for as many runs as goals nest: warm re-solves of one body still allocated a nested-goals list
per run (`docs/v2/plan/lanes/mem/db-solver.md`, item 8). Kept, a pool holds a buffer per level
after one run. Answers are unchanged.

## Dropped since the previous base (ra-ap-rustc_next_trait_solver 0.166.0, rustc `ce89c898`)

- The backport of rust-lang/rust#161380 (a constant sends a goal back to the caller's typing
  mode only when it names an opaque type or under `generic_const_exprs`): upstream has it.
- The backport of rust-lang/rust#159228 (an overflowing root goal is retried with twice the
  recursion limit, also for proof trees): upstream has it, and with it the
  `emit_next_solver_overflow_fcw` warning blaze's backport left out.
- The `[const] Iterator` hunk of the old `unimplemented!` patch: upstream answers `NoSolution`.
- The reservation-impl hunks of that patch (`NoSolution` outside coherence, ambiguous in it):
  upstream removed `ImplPolarity::Reservation`, so no impl has that polarity any more.
- `#![forbid(unsafe_code)]`: now `deny` (patch 0002), for the reason given there.
