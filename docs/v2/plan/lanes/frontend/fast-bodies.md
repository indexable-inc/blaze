---
status: plan
summary: frontend-fast-bodies — which bodies are checked, where and when (fast.md section 5): every crate-wide read a body makes, audited from the code and narrowed so an edit re-infers only the bodies it can change; bodies as plane tasks, reached ones first; a dependency's check outcome computed once for everyone.
date: 2026-10-29
---
# frontend-fast-bodies: which bodies are checked, where and when

Section 5 of [fast.md](fast.md). Costs are counted from the code as of 2026-10-29, not measured
(nothing builds yet); the machine is the one at the top of
[../db/speed-of-light.md](../db/speed-of-light.md). Parts 2 and 3 are built by two children,
whose own notes are [fast-bodies-tasks.md](fast-bodies-tasks.md) and
[fast-bodies-cache.md](fast-bodies-cache.md).

## The work and its bound

- **An edit.** The bodies an edit can change are those whose own text changed and those that
  read a fact the edit changed (a signature, a name in scope, an impl bucket they asked of). The
  bound is to re-infer exactly those and to verify each other body's memo by its edges: one deep
  verify of a body whose reads are all narrow is its edge count times about 11 ns
  ([../db/speed-of-light.md](../db/speed-of-light.md), "verify, per edge"). A typical body has
  30 to 80 edges, so about 0.3 to 0.9 µs per untouched body, against about 30 µs to infer it
  again today (105K cycles, [../db/speed-of-light-infer.md](../db/speed-of-light-infer.md)). A
  crate of 5,000 bodies costs about 3 ms of verification on one core, 0.1 ms on 32, plus the
  bodies the edit really reaches.
- **A cold build.** Every body of every crate the build compiles is checked (rustc's rule, kept:
  part 2), so the bound is the total check work divided by the cores, or the critical path,
  whichever is longer. The critical path is the reached bodies' checks feeding codegen of the
  functions they are, so reached bodies go first.
- **A dependency.** A crates.io dependency at a given version, with a given toolchain and lint
  flags, has one check outcome. The bound is to compute it once for everyone and read it after:
  one store read of a few KB per crate (part 3).

## 1. A body reads only what it names

### What a body reads today

A body's inference (`infer_body_query`, `crates/hir-ty/src/infer.rs`) reads its own HIR and
signatures through per-item queries, and the crate's facts through firewall queries that
backdate when unchanged. Lane items and lane db already narrowed most crate-wide reads: method
probing asks `InherentImpls::in_crate_for_self_ty` and `in_block_for_self_ty` per simplified
self type (`method_resolution/probe.rs`); the solver's impl lookup asks
`TraitImpls::in_crate_and_deps_for_trait_and_self_ty` and `blanket_in_crate_and_deps` per
trait and self type (`next_solver/interner.rs`, `for_each_relevant_impl` and
`for_each_blanket_impl`); trait items by name go through `in_crate_trait_value_items`; names
through `ModuleScope::name`, one query per module and name (`hir-def/src/nameres.rs`). Each of
those buckets reads a crate-wide value (`TraitImpls::for_crate` and the rest), which reruns on
an edit of the crate, and is itself rerun and backdated: an edit costs one rerun of the
crate-wide value plus one cheap rerun per bucket anyone read, and no body.

The audit below lists every crate-wide read left on the paths of `infer`, `method_resolution`,
the solver glue (`next_solver/interner.rs`, `traits.rs`) and `hir-ty-borrowck`, found by
reading each crate-level query those paths call (`rg` over `crate_def_map`, `lang_items`,
`all_crates`, `for_crate`, `for_crate_and_deps`, `for_each_crate_and_block`, `loaded_crates`
and the batch queries, each call followed to its caller).

| read | where | what changes it | so today | design |
|---|---|---|---|---|
| the crate's whole `DefMap` value | every resolver (`hir-def/src/resolver.rs`, `ModuleId::resolver` reads `def_map`) and `DbInterner::new_with` (`next_solver/interner.rs`, `recursion_limit`): both read `crate_local_def_map(..).def_map(db)`, one tracked field | `DefMap`'s derived `PartialEq` covers `names` (what each import resolved through: every `use` added changes it), `diagnostics` and `diagnostics_by_module` (an unresolved import), `derive_helpers_in_scope` (every item with `#[derive(Default)]` or a derive with helpers) | **adding a `use` anywhere in the crate, or a derived item, re-infers every body of the crate**, and reruns every signature query that made an interner | hand the four record fields to a tracked struct of their own at `hand_over_scopes`, as the modules and scopes already are (request R1); the `DefMap` value then changes only with what a body can observe: root, prelude, `macro_use` prelude, the crate's features, edition and limits |
| `recursion_limit` through the `DefMap` | `DbInterner::new_with` | as above, and the crate's features | every solver user (bodies and signatures) depends on the whole `DefMap` | a firewall query `crate_recursion_limit(db, krate) -> u32` (request R2) |
| every module scope of every crate | `probe_all_traits`, `infer/sized_bound.rs` (rustc's `ProbeScope::AllTraits`, for `MethodCallIllegalSizedBound`) | any item added to any crate | a body with that error re-infers on every edit anywhere | per crate, the per-name bucket `in_crate_trait_value_items(db, krate, name)` that probing already uses (request R3) |
| the traits in scope of the module, as one set | `ModuleScope::traits` through `Resolver::traits_in_scope`, read by method probing and operator lookup | a trait declared or imported in the module | a `use` of a trait re-infers every body of that module, whether or not a method of it is named | per (module, method name): the traits in scope with a value item of that name. This is `frontend-fast-method`'s interned scope sets (`scope_sets.rs`, [fast-method.md](fast-method.md)); asked of it (request R4), not built here |
| lang items as one value | `DbInterner::lang_items` (`hir_def::lang_item::lang_items(db, krate)`) | a `#[lang]` item added or removed | nothing in practice | **kept**: lang items are declared only by crates with `#![feature(lang_items)]` (core, alloc, std, `no_core` crates), whose edits change the meaning of nearly every body anyway; the value backdates on every other edit (`crate_lang_items` reruns per edit of the crate and is equal). A query per lang item would put a memo lookup (5.5 ns) where the solver now loads a field, hundreds of times per body |
| a dependency's signature batches | `lower.rs` (`impl_trait`, `impl_self_ty`, field types, impl predicates) through `CrateImplTraits::of` and the rest (`lower/batch.rs`) | an edit of any item of that kind in that crate | nothing: only crates that are not `Local` are batched, and they are not edited | **kept**; an edited path dependency outside the workspace reruns its batch per edit, which `batched_crate`'s doc already names |
| `loaded_crates`, `crates_containing_incoherent_inherent_impls`, `define_opaque_functions` through `defining_functions`, `trait_value_items` through `in_crate_trait_value_items` | `method_resolution.rs`, `opaques.rs` | the crate graph, or the crate's items | nothing: each is read through a firewall that backdates | kept |
| `TraitImpls::for_block`, `InherentImpls::for_block` | `for_each_block_trait_and_type`, `for_each_block`, `for_each_crate_and_block` | an impl in that block | only bodies of that block's body | kept: a block is inside one body |
| `trait_environment_query`, `generic_predicates_of`, `fn_sig_for_fn` | per item | that item's signature | correct: these are what a body names | kept |
| `hir-ty-borrowck` | reads `lang_items` (as above), the body's MIR, `InferenceResult`, per-item signatures and layouts | | nothing crate-wide beyond lang items | kept |

`krate.data(db)` (edition, origin) and `all_crates` are inputs of the crate graph, which
changes only when the workspace's crates do.

### What each edit re-infers once R1 to R3 land

The tests (`crates/hir/src/tests/bodies.rs`) state it per kind of edit, as the existing
incremental tests do (`log_executed`, counting `infer_body_query` and `borrowck`):

- editing one function's body re-infers that body alone, and borrow-checks it alone;
- adding an impl of a trait for a type no other body asks about re-infers no body;
- adding a struct, an enum with `#[derive(Default)]`, or a function re-infers no body;
- adding a `use` of a struct, in the bodies' own module or another, re-infers no body;
- adding an unresolved `use` re-infers no body (it is a def map diagnostic only);
- adding a trait, or a `use` of one, re-infers no body of another module. In its own module it
  re-infers at most that module's bodies: inference reads the module's traits in scope as one
  set when it starts (`infer.rs`, `traits_in_scope: resolver.traits_in_scope(db)`), which R4
  narrows to the traits with an item of each name probed;
- editing a crate a body depends on, away from the items it names, re-infers no body of the
  dependent crate;
- changing the signature of a function a body calls re-infers that body, and dropping an impl a
  body calls into re-infers it (the cutoff is not a blind one);
- a warm check of every body (all memo hits) allocates nothing (`blaze_alloc_count`).

### Prior art

rustc's red-green graph has `typeck` per body, but it reads crate-wide `crate_inherent_impls`
and `trait_impls_of` (per trait, all crates), and resolution runs whole per crate before any
query, so a `use` added re-typechecks every body whose `hir_owner` hash moved and validates the
rest against the whole resolution output. rust-analyzer's `infer` reads `crate_def_map` whole,
so any item change re-infers. Roslyn rebinds a method body only when its own text changed,
and binds declarations lazily per symbol; that is the bound this matches, with the bucket reads
finer than Roslyn's per-type member tables.

## 2. Bodies as plane tasks, reached ones first

Child `frontend-fast-bodies-tasks` ([fast-bodies-tasks.md](fast-bodies-tasks.md)).

- **Today.** `hir::check::check_crate` (`crates/hir/src/check.rs`) groups file modules into
  runs of 32 items and runs them through `Db::run_all`; a build's unit (`blaze-cargo`'s
  `serve/unit/compile.rs`, `make()`) checks its whole crate with
  `blaze_diagnostics::check_crate`, then starts codegen.
- **Design.** One plane task per body (per module for the item-level checks), diagnostics in a
  slot per body read in def-map order, so the output is the same whatever the scheduling. A
  build's reached bodies (the codegen driver's roots and what they reach) are checked at the
  plane's high priority, on the critical path to codegen; every other body of the crate at low
  priority, concurrently.
- **What rustc behavior this keeps, and an objection to fast.md's wording.** fast.md and this
  lane's brief say a build "checks only the bodies reachable from what it builds", leaving the
  rest to `check`. rustc rejects a crate with a type or borrow error in a function nothing
  calls, in `cargo build` as in `cargo check`; checking only reached bodies would make blaze
  accept programs the pinned nightly rejects, which neither superset-of-rust nor the
  mandate allows (it is not opt-in). So reachability decides the **order** of checks, never
  whether a body is checked: a unit's outcome waits for every body, its outputs are withheld
  until then, and unreached bodies fill idle cores while codegen runs. The cost over
  "reached only" is total work, not latency: the unreached bodies' checks run off the critical
  path. The one body not checked is one whose outcome is read from the global cache (part 3),
  which is the same outcome. Sent to `frontend-fast` as an objection (`v2/requests/frontend-fast.md`).

## 3. Dependencies checked once for everyone

Child `frontend-fast-bodies-cache` ([fast-bodies-cache.md](fast-bodies-cache.md)).

- **Key.** frontend-deps' image key for the crate ([deps.md](deps.md), `deps/key.rs`: the
  crate's content, files, dependencies' keys, toolchain and `build`, paths relative), plus what
  a check reads beyond it: the check's producer version, the lint flags (`[lints]`, `RUSTFLAGS`,
  `--cap-lints`) and the borrow checker's mode. Crates frontend-deps refuses an image are
  refused here for the same reason.
- **Value.** The crate's outcome (its diagnostics as the build shows them, failed or not), and
  per body what codegen of it reads, in stable form (ids relocated through a table as the
  signature records are; `InferenceRecord` today names salsa ids and reads back only in the
  database that wrote it).
- **Hook.** A build of a crate whose dependency's outcome is cached reads it and checks none of
  that dependency's bodies; a miss checks and stores.

## Proof

- `crates/hir/src/tests/bodies.rs`: the incremental tests above, one per kind of edit.
- `harness/v2/frontend/cases/bodies/` (58 cases): programs whose outcome depends on which
  bodies are checked and when. Errors in bodies nothing reaches, which rustc rejects (type
  errors, borrow errors, exhaustiveness, resolution, in free functions, methods, trait
  defaults, `Drop` impls, generic functions never instantiated, closures, nested and `async`
  functions, consts, statics, discriminants and associated consts); errors in bodies reached
  only through a function pointer or a trait object; the one kind of error that does depend on
  reachability, a post-monomorphization const panic, rejected when instantiated (040) and
  accepted when not (041); and accepted programs: `cfg`'d-out bodies, macros never expanded,
  unreached bodies that only warn, and bodies reached through a function pointer, a trait
  object, a generic instance, a returned closure, const evaluation, a static, `Drop` and an
  operator impl. `frontend-fast-diff`'s harness runs them through rustc and blaze.
- The children's tests, named in their notes.
