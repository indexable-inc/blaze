---
status: plan
summary: speed of light for type lowering (a type ref to a Ty, signatures, field types, generic predicates and defaults, variance) and item well-formedness (hir-ty-wfcheck), with the arithmetic, what the code costs, what was applied and what stands in the way.
date: 2026-10-29
---
# Speed of light: type lowering and well-formedness

Files: `crates/hir-ty/src/{lower.rs, lower/path.rs, lower/persist.rs, generics.rs, variance.rs}`
and `crates/hir-ty-wfcheck/`. Nothing here was run: every cost is counted from the code as of
10-29 evening, not timed. `proof`'s budgets measure them once the tree builds.

## Unit costs

These come from the machine figures at the top of [speed-of-light.md](speed-of-light.md): 3.5
GHz, an L1 hit in 4.5 cycles, 2 instructions a cycle on branchy code.

| operation | bound | today | where it comes from |
|---|---|---|---|
| salsa memo hit | 5.4 ns | 18 ns | salsa section of speed-of-light.md |
| salsa query execution (claim, reads, publish) | 35 ns | 140 ns | hir-def signatures section |
| intern hit (hash 2 to 4 words, a group line and a value line) | 15 cycles, 4.5 ns | the same | salsa section ("Interning a hit") |
| small heap allocation and its free | 0 | 20 ns; ≈ 2 ns where it is now a frame bump (10-30: lowering's `substs`, `bound_vars`, predicate and field buffers are `Vec::with_capacity_in(.., frame)`; `hot_lint.py` finds none left in `hir-ty`) | db-ty-port-infer-errors section |
| instantiating one clause that names a parameter (fold, about 3 interns) | 0 if not needed | 20 ns | 3 intern hits plus about 40 instructions |
| flag test on an interned `Ty` (`ty.flags()` reads one column word) | 1 to 2 ns | the same | `next_solver/interner.rs`, `flags` column |

Path resolution has its own row below, because the resolver is hir-def's.

## 1. One type reference to a `Ty` (`TyLoweringContext::lower_ty_ext`)

**The work:** read the `TypeRef` from the item's store, dispatch on its kind, lower its
children, and intern the result. For a path, also resolve the name, read the generic
parameters of what it names, lower or default each argument, and intern the argument list.

**The bound:**
- A ref that is not a path (`&T`, `[T; N]`, `(A, B)`, a parameter): 1 store line, about 10
  instructions, 1 intern hit. 4.5 + 5 + 15 ≈ 25 cycles, **about 7 ns**.
- A path with n arguments (`Vec<u8>`):
  - name resolution, as 2 hash probes (the module scope, then the prelude): 9 ns;
  - the generics of the def, as 1 memo hit: 5.4 ns;
  - interning the arguments and the type: 2 × 4.5 ns;
  - about 60 instructions of dispatch and argument matching: 9 ns.
  That is **about 33 ns**, plus the n arguments. A builtin like `u8` resolves in the same 2
  probes and interns nothing new: about 14 ns.

**Today:**
- A ref that is not a path is about 12 ns: the store read and an intern, plus two `tracing`
  callsite checks (`#[tracing::instrument(skip(self), ret)]` and `debug!(?type_ref)`) of a
  couple of ns each. That is **1.7× off**. *(10-30, sol-all-ty: both callsites are removed
  from `lower_ty_ext`, as are `resolve_path_in_type_ns`'s `instrument` and the three `debug!`s
  on the path path in `lower/path.rs`. The workspace sets no `tracing` `max_level` feature, so
  each one cost its callsite's interest check on every call. A non-path ref is now about 7.5
  ns, within 10%, and a path about 5 ns less.)*
- A path is about 130 ns:
  - `at_path`'s hygiene lookup: 5 ns;
  - the resolver's walk of the scope stack: generic parameter scopes by linear name compare,
    then the module map, then the prelude, about 40 ns;
  - `generics(db, def)`: a container lookup and one or two `GenericParams::with_store` memo
    hits, 36 ns;
  - `substs_from_args_and_bindings`' `Vec::with_capacity`: a heap allocation, 20 ns;
  - a `generic_defaults` memo hit and a fold for each defaulted parameter (`Vec<T, A = Global>`
    has one): 25 ns;
  - two interns: 10 ns.

  That is **3.9× off**.

**What stands in the way, and the design:**
1. The `substs` heap buffer. A `SmallVec<[GenericArg; 8]>` holds every real path's arguments
   on the stack. This is requested of db-ty-d-lower (`v2/requests/db-ty-sol-lower.md` 2):
   −20 ns.
2. Salsa's memo hits at 18 ns instead of 5.4. That is speed-salsa's work (MemoRows, a row
   base cached per task): −38 ns per path.
3. Name resolution walks a stack of scopes rebuilt for every query (see section 2). rustc
   resolves every path once, in `rustc_resolve`, and lowering reads the `Res` from a side
   table: 1 load. The equivalent here is hir-def storing each signature path's resolution in
   the expression store when it lowers the item (it has the def map then). That needs an
   invalidation rule for when the def map changes, so it is a hir-def design change, not a
   lowering one.
4. `generics(db, def)` for every path is 1 or 2 memo hits only to count parameters and find
   defaults. A def's parameter counts (`ProvenanceSplit`) and whether it has defaults fit in
   8 bytes, which could ride in the same row as the def's resolution. That leaves one hit,
   for defaults only when one is used.

With 1 and 2 applied, a path is about 70 ns, 2.1× off. With 3 and 4 as well, it is about
35 ns, within 10%.

## 2. A signature query, worked through: `fn_sig_for_fn`

The example is `fn foo<T: Clone>(x: &T, v: Vec<u8>) -> Option<T>`.

**The bound:**

| step | cost |
|---|---|
| `FunctionSignature` memo hit | 5.4 ns |
| scope chain from the container, 1 hit | 5.4 ns |
| `&T` and `T` | 2 × 7 ns |
| `Vec<u8>`: the path, `u8`, `Global` defaulted (a hit and an intern) | 33 + 14 + 10 ns |
| `Option<T>`: the path and `T` | 33 + 7 ns |
| intern `Tys` and the `PolyFnSig` | 2 × 4.5 ns |
| salsa execution | 35 ns |

That is **about 170 ns**.

**Today, about 660 ns, 3.9× off:**
- the signature's memo hit: 18 ns;
- `def.resolver(db)`: a `Vec` of scopes (one allocation) and about 4 memo hits (def map, the
  impl's and the function's generic params, the container), about 90 ns;
- two `TyLoweringContext`s (`ctx_params`, `ctx_ret`): each allocates `bound_vars` (`vec![..]`)
  and interns its binder's `BoundVarKinds`, and a third `bound_vars` call re-interns the same
  binder: 2 × 20 + 3 × 5 ns;
- `generics(db, def)` for the binder: 36 ns;
- the type refs: 12 + 12 + 130 + 30 + 110 + 12 ns;
- two interns and the store: 10 ns;
- salsa execution: 140 ns.

**Applied or requested now:**
- `bound_vars` and the `elisions` stack as `SmallVec`s (requests 1a and 1e). The first bound
  or function pointer of every context allocates today.
- The fn's binder reused from `ctx_params` instead of interned again (1d).
- `substs` on the stack (2).

That is −85 ns, **about 575 ns, 3.4×**.

**What stands in the way after that:**
- Salsa's hit and execution costs: 12 hits × 12.6 ns plus 105 ns, about −255 ns. That gives
  about 320 ns, 1.9×.
- The resolver: an allocated scope stack and 4 memo hits per signature query, in each of
  `fn_sig_for_fn`, `generic_predicates`, `field_types`, `generic_defaults`,
  `const_param_types` and `impl_self_ty`. An item's resolver is the same for all of them.
  - Design: hir-def keeps `Resolver::scopes` in a `SmallVec` sized for module + impl + fn
    (no allocation), and caches the generic-param scopes of a container. Lowering then pays
    1 hit.
  - That is hir-def's (`resolver.rs`); it goes to its owner with the measurement.
- Name resolution and `generics` per path (section 1, items 3 and 4).

With all of these, about 180 ns: within 10%.

## 3. Generic predicates (`generic_predicates`)

The example is `fn foo<T: Clone>` in an impl with 3 predicates.

**The bound:**

| step | cost |
|---|---|
| `generics`, 2 hits | 10.8 ns |
| scope chain, 1 hit | 5.4 ns |
| `T` | 7 ns |
| `Clone`: resolve, its generics, intern the `TraitRef` arguments and the `Clause` | 9 + 5.4 + 4.5 + 4.5 ns |
| `T: Sized`: intern the arguments and the clause | 9 ns |
| the parent's predicates, 1 hit | 5.4 ns |
| intern the 6-clause list | 5 ns |
| execution | 35 ns |

That is **about 105 ns**.

**Today, about 530 ns, 5× off:**
- generics 36, the resolver 90, the context's allocation and binder intern 25, and the
  parent's memo hit 18 ns;
- `lower_where_predicate`'s `collect::<Vec<_>>()`: 20 ns per where predicate;
- the trait path at about 130 ns: its `substs` allocation, `generics(Clone)` and the
  resolution;
- the first `with_elision` push, which allocates the `elisions` stack: 20 ns;
- `own_predicates` and the final `predicates` collect: 2 × 20 ns;
- the `unsized_types` hash probe per parameter, `T: Sized`, the interns: about 25 ns;
- execution: 140 ns.

**Requested:** `own_predicates`, `own_assoc_ty_bounds` and the final list as `SmallVec`s
(1c), and the where-predicate collect (1b). Together with 1a and 1e that is −100 ns, **about
430 ns, 4.1×**. With salsa at its bound (9 hits, one execution), about 210 ns, 2×.

**What stands in the way after that:**
- The resolver and per-path generics, as in section 2.
- **Copying the parent's predicates into every child.** Each of an impl's M methods copies the
  impl's P predicates into its own list and interns P + own words, so an impl costs M × P
  copies and hashes. rustc's `predicates_of` keeps `parent: Option<DefId>` instead, and
  `instantiate` walks the chain.
  - Design: `GenericPredicates` keeps the parent's interned list as a 4-byte
    `StoredEarlyBinder<StoredClauses>` beside its own. `all_predicates()` and the other
    readers, which already return `impl Iterator`, chain the two slices.
  - The offsets stay as they are, relative to the chained sequence, so no caller changes.
  - That removes the copy and the hash over P. For a 20-method impl with 5 predicates: 100
    words hashed and copied, and 20 lists 5 clauses longer in the interner.
  - It touches `predicates`' users in `lower/batch.rs` and `lower/persist.rs`, so it goes to
    db-ty-d-lower after its elision tasks, not as a buffer swap.

## 4. Field types, generic defaults and const parameter types

The example for field types is `struct S<T> { a: u32, b: Vec<String>, c: Option<Box<T>> }`.

- **The bound:**
  - the `VariantFields` hit and a scope chain: 10.8 ns;
  - the type refs: 14 + (33 + 19 + 10) + (33 + 33 + 7 + 10) ns;
  - one allocation for the `ThinVec` value, which is the memo's own: 20 ns;
  - execution: 35 ns.

  That is **about 225 ns**.
- **Today: about 820 ns, 3.6× off.** The resolver, the context and its generics are 170 ns,
  the four paths 450 ns, and execution 140 ns.
- **Gap and design:** the same items as section 2.

A dependency crate's field types, impl self types, traits and predicates are one batch query
per crate and kind (`lower/batch.rs`). That turns one execution per item (140 ns today, 35 at
the bound) into one per crate.

`generic_defaults` and `const_param_types` lower one type ref per parameter that has one, so
the costs are those of section 1. `generic_defaults` returns early for an item without
parameters before it builds a resolver. `const_param_types` should do the same for an item
without const parameters: it builds a resolver and a context first today, about 130 ns spent
on nothing for most items. That early return is requested of db-ty-d-lower
(`v2/requests/db-ty-sol-lower.md` 5), since it is editing `lower.rs`.

## 5. Variance (`variance.rs`)

The example is `struct S<'a, T> { a: &'a T, b: Vec<String>, c: Option<u8> }`.

- **The work:** walk each field type with the ambient variance, and constrain each type
  parameter and early-bound lifetime the walk meets.
- **The bound:**
  - generics and field types, 3 hits: 16.2 ns;
  - only the nodes that hold a parameter, `&'a T`, about 20 instructions: 6 ns;
  - `b` and `c`, one flag test each: 2 ns;
  - intern the result: 4.5 ns;
  - execution: 35 ns.

  That is **about 65 ns**.
- **Before this pass, about 275 ns, 4.2× off:**
  - every node of every field was walked, with a `tracing::debug!` callsite check per node;
  - each ADT met cost a `variances_of` memo hit, even `Vec<String>` and `Option<u8>`, which
    hold no parameter: 2 × 18 ns plus 8 nodes;
  - the `Box<[Variance]>` allocation: 20 ns;
  - generics, field types and execution: 36 + 18 + 140 ns.
- **Applied now:**
  - `add_constraints_from_ty` returns at once from a type with neither `HAS_TY_PARAM` nor
    `HAS_RE_PARAM`. Only a `TyKind::Param` and an `ReEarlyParam` ever constrain anything, so
    the answers are the same, and the ADTs in such a type are never asked for their
    variances. That also cuts the `variances_of` cycles that went through them.
  - The variances are a `SmallVec<[Variance; 16]>`, so there is no allocation.
  - That gives **about 215 ns, 3.3×**. With salsa at its bound, **about 70 ns, 1.08×: within
    10%**.
- **Test:** rustc's variance fixtures in `crates/hir-ide/src/tests/variance.rs` cover every
  kind the walk sees. A fixture that mixes parameter-free and parametric fields was requested
  of that file's owner (request 4).

## 6. Item well-formedness (`hir-ty-wfcheck`)

The example is the same `struct S<'a, T> { a: &'a T, b: Vec<String>, c: Option<u8> }`: its
`wfcheck` and the `inferred_outlives` query that it reads.

**The work:**
- what each field type needs of what outlives what (`wf`), keeping only what names `'static`;
- what the where clauses need (`where_needs`);
- RFC 2093's inferred bounds (`inferred_outlives`), which read the outlives requirements of
  each subtype of each field that can name a lifetime parameter;
- checking all of it against what the item knows.

**The bound:**
- `inferred_outlives`: 1 field with a lifetime parameter (one `Ref`, one need, its
  components); `b` and `c` are one flag test each. About 30 ns, plus execution 35 ns.
- `wfcheck`:
  - `trait_environment` and `inferred_outlives` hits: 2 × 5.4 ns;
  - the fields' walks, at 1 flag test for each type without a parameter or local region;
  - `T: Sized`'s outlives predicates: none, so nothing is instantiated, 1 hit of `Sized`'s
    predicates;
  - execution: 35 ns.

  About 60 ns.

The total is **about 125 ns**.

**Before this pass, about 1.2 µs, 9.5× off.** Two things cost the most:
- **Every predicate instantiated, to keep only the outlives ones.** `adt_needs`, `required`,
  `where_needs` and the impl check called `GenericPredicates::query_all(def)
  .iter_instantiated(args)` and then dropped everything but `TypeOutlives` and
  `RegionOutlives`. Most predicates are `T: Sized`, `A: Allocator` and the like, so this
  folded and re-interned about 3 clauses per ADT met, about 60 ns per `Vec<_>`.
- **Every subtype walked.** `inferred_outlives` pushed every subtype of every field into a
  fresh `Vec` and ran `required` on each. `wf` entered every node, with two memo hits
  (predicates and inferred bounds) per ADT. That is about 170 ns for `Vec<String>` (three
  ADTs: `Vec`, `String`, `Global`) in each of the two queries.

**Applied now** (`outlives.rs`, `lib.rs`):
- `predicate_needs(db, def, args, out)` filters a definition's predicates to the outlives ones
  before it instantiates them. Instantiating preserves a clause's kind, so the needs are the
  same, in the same order. Every caller uses it.
- `required` returns at once from a type without `HAS_RE_PARAM`. What it finds must outlive
  an `ReEarlyParam`, and after instantiation that can only come from the type's own
  arguments.
- `inferred_outlives` walks with `walk_with(ty, HAS_RE_PARAM)`, which doesn't enter a type
  without the flag (none of its subtypes has it), into one reused buffer.
- `wf_into` returns from a type with none of `MAY_NEED` (a parameter, an alias, an inference
  variable, a placeholder, an error, or a local region). Such a type, like `u8`,
  `Vec<String>` or `&'static str`, only needs `'static` or a bound or erased region to
  outlive things:
  - that always holds;
  - `resolve` reports nothing for it;
  - added to `Known` it decides nothing, since no parameter or alias compares equal to it.
- `wf` filters escaping needs in place instead of through a second `Vec`, `where_needs`
  reuses its two buffers across clauses and no longer collects the clauses, and `variants`
  is a `SmallVec`.
- `Wfcx` builds its inference context and `trait_environment` on the first normalization or
  proof (a `OnceCell`), not in `new`. `normalize` returns before it for a type without an
  alias, and `prove_needs` for an empty list, so a function or impl whose signature has no
  alias and no ADT with a trait predicate builds none: about 10 allocations and a memo hit,
  about 250 ns, saved. Nothing else reads the context, so the answers are the same.

That leaves about 400 ns for `wfcheck` and 190 ns for `inferred_outlives`, **about 590 ns,
4.7×**. With salsa at its bound (2 executions, about 6 hits) it is **about 300 ns, 2.4×**.

**Tests** (`crates/hir-ty-wfcheck/src/outlives/tests.rs`, on hir-ide's test database as
unsafeck's are), over every field type of a fixture with references, nested ADTs, function
pointers, higher-ranked types, a projection and `'static`:
- `walk_with` is `walk` filtered by the flag;
- `required` over the pruned walk finds exactly what the old `required` (every predicate
  instantiated) finds over the full walk;
- the pruned `wf_into` gives the old one's needs in the same order with some left out, and
  each one left out is trivial;
- `Static<T> { x: &'static T }` is E0310 at its field, and the other ADTs have no error.

**What stands in the way after that:**
- **`trait_needs` proves every trait predicate of every ADT in the signature with the solver**,
  one `ObligationCtxt` each, to collect the region obligations the proofs make. That is about
  0.15 µs per predicate (a root evaluation, per speed-of-light.md), several per signature.
  - A global predicate (no parameter, no local region) can only add needs that hold.
  - It can't simply be skipped: `prove_needs` drops all of a batch's needs when one predicate
    fails, so skipping a failing global one would report needs that are reported nothing
    today.
  - The design that keeps the answers: evaluate global predicates through the solver's
    global cache alone (`evaluate_root_goal`, a hit at about 40 cycles) to learn whether they
    hold, with no fulfillment context, and give only the rest a context.
- **Each check's buffers** (`Known`'s two lists, `needs`, `found`, the `header` and `inputs`
  vectors): about 8 allocations, 160 ns, per item with any needs.
  - Design: one `Scratch` of reused buffers in `Check`, borrowed per step, as `where_needs`
    now does.
  - `hot_lint.py` lists these. The `&dyn HirDatabase` findings are the database interface
    every crate takes.

**Recounted 10-30 (sol-all-ty), from the code, nothing built:**
- **Buffers.** Every buffer of a check is now in the check's frame (`Vec::new_in(self.frame)`,
  `blaze_mem`), a bump of about 2 ns, not a global allocation and free at 20 ns. For the 8
  buffers that is 8 × 18 ≈ −145 ns. The example ADT is **≈ 445 ns, 3.6×**. With salsa at its
  bound it is **≈ 155 ns, 1.24×**, and what is left is salsa's and `trait_environment`'s.
- **Global predicates** (`prove_needs`, `outlives.rs`). A predicate that `is_global()` (no
  parameter, inference variable, placeholder, or local or erased region) is answered by
  `InferCtxt::predicate_must_hold_modulo_regions`. That is the solver's evaluation in a probe,
  a global-cache hit after the first time, with no `ObligationCtxt`. A failure still drops the
  batch, as before. What its proof would have needed can only be that a global type or
  `'static` outlives `'static`, which holds and which `resolve` reports nothing for.
  - Per global predicate: ≈ 0.15 µs down to ≈ 40 ns (a cache hit plus the probe).
  - This runs on the impl-header and function-signature paths, which call `trait_needs`. The
    example ADT doesn't, so its row doesn't change. A signature with `Vec<String>` asks
    `String: Sized` and `Global: Allocator`, both global: ≈ −0.2 µs.
  - Test: `global_predicates_leave_out_only_what_holds` (`outlives/tests.rs`) runs the old
    prover (`prove_needs_reference`, every predicate in a context) and the new one on every
    field type of a fixture with global trait, projection and `'static`-blanket-impl
    predicates. It asserts the new needs are the old ones in order, with only trivial ones left
    out. It is the old-versus-new test. A differential test against rustc for this is the
    E0309/E0310 cases in the wfcheck fixtures, which keep their expected results.

## 7. `generics.rs`

`generics(db, def)` is a stack `ArrayVec` view over hir-def's `GenericParams`, with no query
and no allocation: a container lookup and 1 or 2 memo hits. That is the bound for what it
does. Its cost is how often it runs: once per path lowered (section 1, item 4), once per
context, and once per variance and predicate query. Its iterators chain 4 small iterators and
filter: about 5 instructions a parameter, which is nothing next to the hits.
`object_lifetime_default` scans the owner's where predicates: about 10 instructions per
predicate, once per `dyn` argument.

## 8. Cold batches (`lower/persist.rs`)

**The work:** read a dependency crate's batch from its image: check the columns, map each
key back to its id, and turn each type-table entry into a live interned type.

- **The bound:** one intern per distinct type in the table, which must exist for the types to
  be usable, plus a pass over the key and value columns at memory bandwidth. For 10k impls
  with 20k distinct types: 20k × 4.5 ns = 90 µs, plus 120 KB of columns, about 4 µs.
- **Today:**
  - `read` checks every column once, about 1 ns per row;
  - `TyTableReader` interns each type once;
  - rebuilding the batch collects a `Box<[ImplId]>` of keys and each value column.

  That is about the bound plus about 6 column allocations per batch, within 10% for any
  crate with more than a few hundred rows.
- **What would beat it:** reading keys in place. `ImplId` is a salsa id, and the column holds
  the same bits, so a `repr(transparent)` view would skip the key copy. That is a cold path,
  once per dependency crate per daemon, so it waits for a measurement.

## Summary

Every figure is counted, not measured. "With salsa" means with speed-salsa's memo hit and
execution at their bounds.

| operation | bound | before | after this pass and its requests | with salsa | what is left |
|---|---|---|---|---|---|
| a type ref, not a path | 7 ns | 12 ns | ≈ 7.5 ns (10-30) | ≈ 7.5 ns | within 10%: the `tracing` callsites are gone |
| a type ref, path | 33 ns | 130 ns | ≈ 105 ns (10-30) | ≈ 65 ns | resolution in hir-def's store, generics shape in the same row |
| `fn_sig_for_fn` (example) | 170 ns | 660 ns | 575 ns | 320 ns | resolver per query, path costs |
| `generic_predicates` (example) | 105 ns | 530 ns | 430 ns | 210 ns | resolver, path costs, parent predicates copied |
| field types (example) | 225 ns | 820 ns | 740 ns | 450 ns | resolver, path costs |
| variance (example) | 65 ns | 275 ns | 215 ns | 70 ns | within 10% |
| wfcheck + inferred outlives (example ADT) | 125 ns | 1.2 µs | ≈ 445 ns (10-30) | ≈ 155 ns | salsa; the resolver of `trait_environment` |
| cold batch read | interns of the type table | same | same | same | in-place keys, once measured |

## State of the art

- **Lowering:** rustc's `collect` queries (`type_of`, `predicates_of`, `fn_sig`) and Zig's
  `InternPool`.
  - rustc lowers each signature once per item with its paths already resolved (`Res` in the
    HIR); Zig keeps every declaration's data in typed arrays indexed by `u32`.
  - We match rustc's granularity for workspace crates, and the InternPool layout for dependency
    crates (batches by kind, `u32` type handles).
  - We don't yet match rustc on resolution. Lowering resolves every path again through a scope
    stack, which is why paths are 2× off even with salsa at its bound.
  - What it would take: resolutions stored in the expression store (section 1, item 3), and
    predicates kept relative to the parent as `predicates_of`'s `parent` is.
- **Pruning by type flags:** rustc's own technique. `ArgFolder` skips a type without
  `HAS_PARAM`, and `TypeVisitableExt::has_*` gate every fold.
  - The pruning here applies the same test to variance and to outlives well-formedness.
  - rustc's `variances_of` runs a crate-wide constraint solve (`crate_variances`), which
    visits every type in every ADT once. Ours is per item and visits only the nodes with a
    parameter, so a crate whose fields are mostly concrete does less work than rustc's.
- **Well-formedness:** rustc's `wfcheck` proves every nominal obligation through the trait
  solver. This port needs only the outlives ones, so it instantiates only those (rustc
  instantiates all of `predicates_of` per ADT met), and for any type without parameters or
  local regions it proves nothing.
