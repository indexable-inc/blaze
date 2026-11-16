---
status: plan
summary: What stores the type checker's results now that the seven old persistence forms are gone. The body store and the goal store keep records, naming items by `blaze_id::ItemKey`; one type codec (`next_solver::persist`) writes their types. Also covers the type tables (kept process-global, with types written into each record), the `TyWorld` seam for a `blaze-ty` crate, the id-bit census, and why the effects summary is not keyed per function.
date: 2026-11-15
---
# Where the type checker's results are kept

The refit of the parked checker (crates/hir-ty and hir-ty-*) on 2026-11-15 changed no reasoning. It deleted the forms written for the old daemon and the old item layer. What outlives a task now goes through two stores:

- **The body store** (`hir_ty::body_store`) keeps one record per step of a body: its inference, its built MIR, its borrow-check verdict and its constants. A record is filed under the body's identity and its read list.
- **The goal store** (`next_solver::goal_store`, with its gate `goal_store::gate`) keeps one record per root goal, in stable form, with its read list.

Both name items through the driver's facts object: `BodyFacts::def_key`/`def_of` and `GoalFacts::def_key`. These now return `blaze_id::ItemKey`. A plain item is named by `blaze_id::key_of` and read back by `blaze_id::resolve`. The body itself and what it owns (closures, coroutines, opaque types) are named "self", relative to the body (identity.md). No record holds a `Gid`'s bits, a `PortableIds` index or a def-map position.

## The seven forms

| Form (lines before) | Writer and reader | What it stored | How ids were made portable | What carries it now |
|---|---|---|---|---|
| `infer/persist` (5,334) | Written by the body store's offer and the old crate part (`check_cache::part`). Read by `lookup_inference` and `served_inference`. | An `InferenceResult`: types in a type table, adjustments, diagnostics, closure captures. | `id_bits` in a relocating scope gave a table index. Outside a scope it wrote the memo id's session bits. | Kept as the body store's inference codec. The session-bits path is deleted: an id written outside a scope is a panic, and an id read outside one is refused (`IdRefused::NoScope`). |
| `next_solver/persist` (2,973) | Every interner (row tag and home, in memory), plus every record's `TyTableWriter`/`TyTableReader`. | The words of interned values, and a record's type table. | `Sink::id` and `Source::id` through `ids::id_bits`/`id_of`. | **The one type codec**, kept. The interner needs it to work, so it is a fact dependency, not a cache. `check_cache/ids.rs` moved under it as `persist/ids.rs` and `check_cache/port.rs` as `persist/port.rs`. |
| `mir/persist` (2,278 + 1,518 tests) | Written by the body store's built-MIR offer and the old runtime part (`check_cache::runtime`). Read by the same. | A built `MirBody` as `u32` words, its types in a type table. | Through the same relocating table. | Kept as the body store's MIR codec. Its tests (1,518 lines) are deleted: they round-tripped outside a scope, which is the deleted form. |
| `lower/batch` + `lower/persist` (2,285) | Written by the per-crate batched lowering queries. Read by `*_with_diagnostics` in lower.rs. | A whole crate's signature lowering (impl traits, self types, field types, predicates) as one batch, persisted. | Rows ordered by `gid`, items by the crate's frozen indices. | Nothing is kept: the per-item lowering queries compute in the memo. A body record's read list names their facts. |
| `check_cache` (3,562) | Written by blaze-diagnostics (`encode` after a crate's check). Read by `set_served`, then `served_inference`, `served_passed` and `runtime::served`. | A crate's bodies part, filed under its image key: each body's inference record, borrow verdict and runtime MIR entries. | Items as indices in the crate's `PortableIds` (hir-def's frozen encoding). | The body store, per body. The borrow verdict is `offer_passed`/`lookup_passed`. Runtime MIR is made from the stored built MIR. The id table moved to `persist/ids.rs`. |
| `method_resolution/frozen` (2,203) | Written as frozen pages of each crate's impls (inherent and trait) in its image. Read by method resolution. | Impl lists by self type and trait. | Page and frozen indices. | Computed from the crate's tables in the memo (`InherentImpls::for_crate`, `TraitImpls::*`). Nothing is kept. |
| `global_goals` (1,597) | Written by the solver at root-goal evaluation into a process-global goal table and a shape table (auto-trait depth, Copy/Clone impl). Read by the fast path and `holds_at_registration`. | Root goal answers and goal shapes. | Rows in interning order, in memory. | The goal store, through its gate (`goal_store/gate.rs`, the old file moved and cut down). Repeats within one run hit the solver's own cache (`interner::tls_cache`). |

No caller depended on a deleted form for a fact. The one fact dependency found is `next_solver/persist`: the interner computes each row's tag and home with it, so it stays.

## Other forms still in the tree

These were outside the seven and are left as they are:

| Form | Status |
|---|---|
| `hir-ty-wfcheck/src/persist.rs` (553) | Its own codec. |
| `consteval/deps` | A dependency's constants under the crate's check key. It has its own id-free codec. |
| `body_store/konst` | Uses consteval's codec. |
| `hir-ty-codegen/v2/roots/kept/frozen.rs` | Built over hir-def's frozen form. Goes when that form goes, in the hir-def lane. |
| `persist/port.rs` | Codegen's describe records. The caller names items by `blaze_items::item_key`. It should move to `blaze_id::key_of` once hir-def implements it. |

## The type tables

Interned types live in process-global columnar tables (`interner::table::Rows`, the `static` type, constant, region and list tables). A row's index depends on interning order, so it must never appear in a record.

**Option A: keep the tables process-global, and write each record's types into the record.** Each record carries its own `TyTable`: every value written once, after the values it holds, ids through the record's id table.
- The one codec stays in `next_solver/persist` (later `blaze-ty`). There is a single home: `TyTableWriter`/`TyTableReader`, `TABLE_SCHEMA`, `ids` and `port`.
- Cost: a read re-interns the record's types, about one hash probe per distinct value. A record repeats the types it shares with other records.
- Order independence holds: a record's table is in that record's own first-use order, and its ids are its own table's entries. Two databases that build the same body write the same bytes. The one exception is the order of a `dyn` type's projections and auto traits, which follows the writer's ids; the reader sorts them again. That makes a byte difference a miss, never a wrong value. It should become an ordering by `ItemKey`.

**Option B: make the tables a mappable image, as Relax does.** Rows would be mapped read-only from a file shared by processes, and records would name rows directly.
- Cost: row indices become persistent, so the tables need a canonical order: hash-consing by content, with stable indices across builds and machines. Every table needs a generation scheme. Readers would map one large image per build. Garbage collection of dead rows becomes a disk-compaction problem. It also conflicts with "no state between requests" unless the image is itself a content-keyed record.
- Benefit: no re-interning on a hit.

**Recommendation: A.** It needs nothing new. It matches "a command answers and exits". It keeps each record self-contained and valid by its key. The re-interning cost is in proportion to the record, which is within the floor's order for an edit loop where hits are small. B is worth revisiting only if a profile shows re-interning dominating warm builds.

What was implemented for A:
- the session-bits path removed from `ids` and from `TyTableReader`'s `Source::id`;
- `ids` moved beside the codec;
- `ItemSink` documented as the stores' item naming;
- the store traits typed with `ItemKey`.

## Id-bit census (2026-11-15)

Sites counted: `gid()`, `from_gid`, `session_bits`, `gid_bits` and `word_of`.

- hir-ty: 183 sites.
  - **(b) Into a record, converted: 153.** These are `persist/ids.rs` 44, `infer/persist` 60, `port.rs` 21, `body_store/infer.rs` 21, `persist.rs` 4, consteval 1 and the goal_store tests 2. All go through a one-task `Table`, and the record holds keys (`ItemKey` through `ItemSink`) and locations, never bits.
  - **(a) Within one task: 29.** These are method resolution's `pick_cache` 5 and `scope_sets` 4, `display` 4, the arena homes in `mir.rs` 3, `lib.rs` 2 and `tls_cache` 2, the homes of `region`/`predicate`/`consts`/`allocation` 4, and test ids 5.
  - **(c) Ordering by id: 1.** The `dyn` predicate sort in `persist.rs` (`ExistentialPredicates::get_node`).
- hir-ty-codegen: 61 sites.
  - **(a): 54.** These are `from_gid` to reach a closure's location or MIR within a task.
  - **(b): 6.** `described.rs` writes these through `port`, with items by item key.
  - **(c): 1.** `v2/roots.rs:236` sorts the linked crates by `gid`; it should sort by `CrateKey`.
- hir-ty-identity: 21 sites, all (a). Keys are made from content (`key::closure_key`).
- hir-ty-pure: 18 sites.
  - **(a): 15.**
  - **False positives: 3.** `refined.rs` has its own `word_of`, which is pointer width.
  - `walk.rs:570` `step_key` packs a `Gid` into the step counter's key. It is in process only, but it would have to change if steps were ever filed.
- hir-ty-mir: 13 sites, all (a). `inline/cycle.rs` and `specialize.rs` group by id, and nothing reads that order.
- Also class (b) but not converted: the body store's `traces` table (`body_store.rs`, `Lock<HashMap<(DefWithBodyId, Step, SchemaHash), Arc<Traced>>>`). It outlives a `Memo::next` within one process. It should be keyed by the body's identity (`BodyFacts::identity`).

## The effects summary (`hir_ty_pure::walk::summary_in`)

**What it computes.** For one instance (a body with its generic arguments) in its home `ParamEnvAndCrate`, under one proof mode, it gives:
- the effect kinds the instance and its callees may have;
- per effect, the refused kinds and their witnesses;
- the environment variables read;
- its return in terms of its parameters;
- its memory effects;
- every callee, reified pointer and vtable method, in site order;
- the first impure site.

It is one scan of the body's MIR plus a read of each callee's summary. A call cycle iterates to a fixpoint from `Summary::recursive`.

**Keying grain.** It is a memo query keyed by `(home env, Proofs, Instance)`. That means per monomorphized instance, not per function. It is homed by the greatest arena its key names and dropped with that arena.

**Why it is not keyed per function by content.**
1. Its value is transitive: it folds in every reachable callee's summary, and through cycles, a fixpoint. A content key would have to cover the code of everything reachable, not just the body's own text.
2. It is per instance. The same function under other arguments has another value.
3. It holds in-memory ids (`Instance`, `StoredGenericArgs`, `Symbol`) with no portable form.
4. The home env is chosen per crate (`home_env`).

A per-function content-keyed form would split it in two: a local summary of the body alone, keyed by its code identity, and a composition over the call graph that is computed and never stored. Not done here.

## Extracting `blaze-ty`

The goal is a crate that holds the type tables and the solver adapter, so that both the per-body driver (`hir_ty::infer`) and the new driver (`crates/blaze-check`) sit on it. It must not depend on hir-def, base-db or hir-expand. hir-ty re-exports it under `hir_ty::next_solver::*`.

**What blocks a plain move.**
- `SolverDefId` (`next_solver/def_id.rs`) is an enum of hir-def ids and of hir-ty's interned ids. 32 files under `next_solver/` name `hir_def` directly; `ty.rs` does 54 times and `interner.rs` 25 times.
- The table module (`interner::table`, about 3,000 lines inside interner.rs) depends on `crate::evict` (`Released`, `front_ends_kept`) and on interner-level statics (`rows_held_by_process_statics`, `WorldExposer`, `fixed_hash`, `probes`).
- `persist/ids.rs`'s `Relocate` impls are for hir-def types. The orphan rule forbids implementing a `blaze-ty` trait for a hir-def type inside hir-ty. So `Relocate` must take `blaze-ty`'s own id types.

**Design for the ids.** `blaze-ty` owns opaque, concrete id newtypes over `blaze_memo::Gid`: `DefId`, `TraitId`, `AdtId`, `ImplId`, `CallableId`, `ClosureId`, `CoroutineId`, `OpaqueId`, `AnonConstId` and `AssocTermId`.
- Each carries a `DefKind` tag. Kinds that only hir-ty knows (builtin-derive impl, RPITIT, ctor) are opaque `DefKind` variants.
- hir-ty converts with `From`/`TryFrom` at the `TyWorld` impl only.
- An associated type on `TyWorld` was rejected. It would make every type generic (`Ty<'db, W>`) across about 39,000 lines and break the `'static` tables.

**`TyWorld`: the item questions the solver adapter asks.** The `Interner` impl has 118 methods. 83 read the database: 79 are item questions and 4 are memo-engine hooks. Of `SolverDelegate`'s 20 methods, 5 read the database. Grouped:

| `TyWorld` method | `Interner` / `SolverDelegate` methods that use it |
|---|---|
| `generics_of(def)` | `generics_of`, `check_alias_term_args_compatible`, `debug_assert_alias_term_args_compatible`, `unsizing_params_for_adt` |
| `predicates_of(def, which)` | `clauses_of`, `own_clauses_of`, `explicit_implied_clauses_of` (non-trait), `is_fully_generic_for_reflection`; `well_formed_goals` |
| `explicit_super_clauses(trait)`, `explicit_implied_clauses(trait)` | `explicit_super_clauses_of`, `explicit_implied_clauses_of` |
| `item_bounds(def)` (all, self, non-self) | `item_bounds`, `item_self_bounds`, `item_non_self_bounds`; `add_item_bounds_for_hidden_type` |
| `impl_super_outlives(impl)` | `impl_super_outlives` |
| `const_conditions(def)`, `implied_const_bounds(def)` | `const_conditions`, `explicit_implied_const_bounds` |
| `constness(def)` | `fn_is_const`, `impl_is_const`, `closure_is_const`, `alias_has_const_conditions` |
| `type_of(def)` | `type_of` |
| `opaque_hidden_type(opaque)` | `type_of_opaque_hir_typeck`, `type_of` (opaque) |
| `fn_sig(callable)` | `fn_sig` |
| `variances_of(def)` | `variances_of`, `opt_alias_variances` |
| `adt(adt)`: flags, repr, variants, field types, destructor | `adt_def`, `unsizing_params_for_adt`, `reflect_is_enum`; `AdtDef::{struct_tail_ty, all_field_tys, destructor, field_representing_type_info, repr}` |
| `adt_sizedness_constraint(adt, kind)` | `AdtDef::sizedness_constraint` |
| `assoc_parent(term)` | `projection_parent`, `impl_or_trait_assoc_term_parent`, `inherent_alias_term_parent`, `trait_ref_and_own_args_for_alias` |
| `trait_assoc_types(trait)` | `associated_type_def_ids` |
| `has_item_definition(term)` | `has_item_definition` |
| `impl_assoc_item_for`, `specialized_ancestor`, `impl_has_unconstrained_params` | `fetch_eligible_assoc_item` |
| `for_each_relevant_impl`, `for_each_blanket_impl` | `for_each_relevant_impl`, `for_each_blanket_impl` |
| `impl_trait_ref(impl)` | `impl_trait_ref`, `is_fully_generic_for_reflection` |
| `impl_flags(impl)` | `impl_polarity`, `impl_is_default` |
| `impl_specializes(a, b)` | `impl_specializes` |
| `trait_flags(trait)` | `trait_is_auto`, `trait_is_alias`, `trait_is_fundamental`, `trait_is_sealed`, `trait_is_coinductive`, `trait_is_unsafe`, `trait_is_marker` |
| `trait_dyn_compatible(trait)`, `generics_require_sized_self(def)` | `trait_is_dyn_compatible`, `generics_require_sized_self` |
| `supertraits(trait)` | `supertrait_def_ids` |
| `lang_items(krate)` (a table; the `require_*`/`is_*`/`as_*` methods become `blaze-ty` code) | `require_{projection,trait,adt}_lang_item`, `is_{projection,trait}_lang_item`, `as_{projection,trait,adt}_lang_item`, `is_default_trait`, `is_sizedness_trait` |
| `crate_facts(krate)`: recursion limit, features | `recursion_limit`, `features` |
| `has_target_features(fn)` | `has_target_features` |
| `closure_kind(id)`, `coroutine_for_closure` | `coroutine_movability`, `coroutine_for_closure`, `is_general_coroutine`, `coroutine_is_async`, `coroutine_is_gen`, `coroutine_is_async_gen` |
| `coroutine_witnesses(coroutine)` | `coroutine_hidden_types` |
| `opaques_defined_by(body, with_coroutines)` | `opaque_types_defined_by`, `opaque_types_and_coroutines_defined_by` |
| `item_name(def)` | `item_name`, and `Debug` of the ids |
| `const_eval`, `anon_const_is_dependent`, `anon_const_owner` | `evaluate_const` |
| `where_const_condition(env, ct)` | `const_condition` |
| `anon_const_comptime_binders(c)` | `check_alias_term_args_compatible` |
| `same_const_expression(a, b)` | `same_const_expression` |
| `reflect` group (7) | `reflect_members`, `reflect_container_of`, `reflect_is_struct`, `reflect_variant_view`, `reflect_is_memo_point`, `reflect_described_desc`, `reflect_type_of` |
| `layout_size(ty, env, krate)` | `TypingEnv::layout_size` |
| engine hooks: `record`, `replay`, `shared_cache`, `query_cache` | `with_cached_task`, `get_tracked`, `with_global_cache`, `with_global_cache_to_read` |

This list is also what the new driver's `World` must give the solver. `layout` depends on hir-def, so it stays in hir-ty behind `layout_size`.

**Move order.** Each step compiles before the next.
1. Id newtypes and `DefKind` in `blaze-ty`; `SolverDefId` over them.
2. `Relocate` and the scope over `blaze-ty` ids.
3. The tables, with `evict`'s hooks behind a trait.
4. `Ty`, `GenericArg`, consts, regions and predicates.
5. The `Interner` impl through `TyWorld`.
6. The files that only need the moved types: the `infer/` rewrite (union-find, region constraints, canonicalizer, snapshot), fulfill, normalize and opaques.
7. `solver.rs`, then `goal_store`.

**Third-party crates `blaze-ty` would pull.**
- Directly: `tracing`, `either`, `smallvec`, `arrayvec`, `thin-vec`, `bitflags`, `serde_derive`, `la-arena`, and `ra-ap-rustc_index` (0.179, from crates.io).
- Through the vendored `rustc_type_ir`/`rustc_next_trait_solver`: `ena`, `indexmap`, `rustc-hash` and `derive-where`.
- Tree crates: `intern`, `macros`, `stdx`, `rustc_abi`, `rustc_ast_ir` (vendored), and the blaze crates (memo, hash, ids, mem, core, record, query, format, reclaim, hashcons, graph).
