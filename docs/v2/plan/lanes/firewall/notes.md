---
status: plan
summary: lane firewall — a body edit touches only its own function. Every dependency across items reads an item's interface, never its body, but in the cases Rust's semantics need the body (listed with rustc's rule); a harness edits one function body at a time in real crates and asserts that the queries executed again and the code compiled again are exactly that function's.
date: 2026-10-30
---
# Lane firewall: a body edit touches only its own function

Most edits change one function's body. This lane makes that provable: what any query of item
`G` reads of another item `F` is `F`'s interface, never `F`'s body, except where Rust's
semantics need the body; and a harness checks it on real crates, one function body at a time.

Code: `crates/hir-firewall/` (the harness, its fixture tests, the real-crate test and the
bench), `HirInstance::of_body` in `crates/hir-ty-codegen/v2/frontend.rs`, and
`Zalsa::keep_every_value` in `vendor/salsa/src/zalsa.rs`.

## 1. The rule

An item's **interface** is what a user of it may observe without its body: its signature
(generics, where clauses and bounds, parameter and return types as written), its visibility, its
attributes that change what users see (`#[inline]`'s request, `#[track_caller]`,
`#[must_use]`, `#[deprecated]`, `#[target_feature]`, `#[no_mangle]`/`#[export_name]`, `repr`,
`#[non_exhaustive]`), a type's fields, variants and layout, a trait's items, an impl's header.
Its **body** is everything inside the braces of a `fn`, the initializer of a `const` or
`static`, and an enum variant's discriminant expression.

A query keyed by item `G` (or by something interned from `G`'s body: its closures, coroutines,
blocks, anonymous constants, opaque types) may read `F`'s body only through one of the
exceptions below, and each exception reads it through a narrow query keyed by `F` whose value
is the one fact the dependents need, so an edit of `F` that keeps that fact re-executes nothing
of `G` (the value is backdated). The items lane's hashes say the same of codegen
([`../items.md`](../items.md), 6.1 "What a body leaks into its interface").

## 2. Where Rust needs the body: the exceptions

Each case is a fact of `F`'s body that another item's meaning depends on. For each, rustc's rule
(the query that reads the body and what reads that query), and what blaze reads.

| # | case | rustc's rule | blaze: the narrow fact | what an edit of `F` that changes the fact reaches |
|---|---|---|---|---|
| E1 | the hidden type of a return-position `impl Trait` of `F`, and of `async fn F`'s future | `type_of(opaque)` for an RPIT is `find_opaque_ty_constraints_for_rpit`, which reads `mir_borrowck(F).concrete_opaque_types`; everything that reveals the opaque (`TypingMode::PostAnalysis`, codegen, layout) reads `type_of(opaque)`, hashed and green when equal | the hidden type, a query keyed by `F`'s `InternedOpaqueTyId` reading `F`'s inference result and backdating | bodies that reveal the opaque (layout of the value, codegen of instances over it, `PostAnalysis` solving), not every caller: callers type-check against the opaque, not its hidden type |
| E2 | the auto traits of an opaque type of `F` (`Send`, `Sync`, `Unpin`, `Freeze`): auto-trait leakage | selection of an auto trait for `ty::Alias(Opaque)` reveals `type_of(opaque)` (`assemble_candidates_from_auto_impls`, `TypingMode::Analysis` with the defining body's typeck as a dependency); a cycle through it is `E0391` | the goal `opaque: AutoTrait` keyed by the interned goal, reading E1's hidden type | the bodies whose goals ask an auto trait of `F`'s opaque |
| E3 | an `async fn`, `async` block or coroutine's layout and witnesses | `coroutine_layout` from `mir_promoted`/`optimized_mir` of the coroutine; `coroutine_hidden_types` from `mir_coroutine_witnesses(F)` for auto traits | the coroutine's layout and witness types, keyed by `F`'s `InternedCoroutineId`; a coroutine type's identity for codegen is its compiled code ([`hir_ty_share::identity`]) | layout and codegen of every instance whose type arguments name the coroutine (`block_on::<F's future>`, its drop glue) |
| E4 | a `const fn` evaluated elsewhere, a `const` item's or anonymous constant's value, a `static`'s initializer read in a const context, an enum's explicit discriminants | `eval_to_const_value_raw(GlobalId)` interprets `mir_for_ctfe(def)` and `mir_for_ctfe` of every `const fn` it calls; `eval_static_initializer`; `eval_explicit_discr`; all hashed by value | `const_eval`, `anon_const_eval`, `const_eval_static`, `const_eval_discriminant` keyed by the constant, reading the MIR of every `const fn` the evaluation calls, backdating on the value | the constants whose evaluation called `F` re-evaluate (their values backdate when equal); the users of a value that changed |
| E5 | inlining decisions in release | the MIR inliner reads `optimized_mir(callee)` (`Inliner::try_inlining`, `instance_mir`); `cross_crate_inlinable(F)` reads `F`'s MIR size at opt-level > 0; the caller's codegen unit depends on both. At opt-level 0 the inliner is off, and an `#[inline]` function is a local copy in each using CGU | dev (Cranelift, opt-level 0): no function's code builds in a workspace function's body (`Described::built_in` only names `#[inline(always)]` callees of crates no edit reaches). Release: a caller's template is keyed by `Imports::blob_key`, which names every body it imported (`blaze_codegen::summary`) | release: the callers whose import set names `F`'s code hash recompile; dev: nothing |
| E6 | closures and coroutines of `F`'s body that escape it | a closure's signature, kind and captures are part of the parent's `typeck`; a closure type escapes through an RPIT (E1) or a generic instantiation, and the instances over it are collected from `F`'s body (`mono item collection`) | a closure's types are `F`'s; codegen instances whose type arguments name a closure or coroutine of `F` are `F`'s (`HirInstance::of_body`) | those instances, and E1 to E3 where the closure is the hidden type |
| E7 | type-alias `impl Trait` (`#[define_opaque]`) and associated-type `impl Trait` | `type_of(opaque)` for a TAIT reads the borrowck results of every body that defines it | as E1, over the defining bodies `define_opaque` lists | as E1 |
| E8 | which instances exist (monomorphization) | `collect_and_partition_mono_items` walks every reached body's MIR | `Build::reach` walks `Described::refs` of each instance | instances newly reached from `F`'s body are compiled; unreached ones dropped. No other instance's key changes |
| E9 | exported symbols of a library (`reachable_set`) | a private item named in the body of a reachable `#[inline]` or generic function becomes reachable and is exported | none: blaze does not export symbols by reachability (only `#[no_mangle]`, `#[export_name]`, `#[used]`) | nothing |
| E10 | crate-wide lints that read every body | `dead_code` (`live_symbols_and_ignored_derived_traits` reads every body's typeck), `unused_imports` and `unused_macros` (resolution marks uses from bodies), `unused_crate_dependencies`, and `#[expect]` fulfilment of those | blaze-diagnostics computes them per check, outside the query database: no query of another item re-executes; the diagnostics of another item may change | the lint's output only |

Nothing else in Rust needs another item's body: type checking, trait selection, method
resolution, variance, drop check, coherence, layout of non-coroutine types, privacy and
stability are functions of interfaces. Each row of the table is a fixture test in
`crates/hir-firewall/tests/exceptions.rs` (an edit that changes the fact, and the dependents it
must reach, and no others), and the real-crate harness asserts that an edit keeping every fact
reaches nothing else.

## 3. The harness

`crates/hir-firewall` ([`Harness`](../../../../../crates/hir-firewall/src/lib.rs)), over any
database whose files it can set: a test fixture's (`base_db::Db` itself) or a real workspace
lowered as a build lowers it (`load_cargo::v2::lower`, `tests/real.rs`).

1. **Setup.** `Zalsa::keep_every_value(true)`, so an LRU eviction never executes a value again
   that no input of changed. The crate's bodies are listed (every `fn`, `const` and `static`
   declared in its modules, impls and traits; a trait's function without a default has none).
2. **Requests.** What a build asks of the crate: each file module's item checks
   (`hir::check::check_module` with no body); for each body, its check
   (`hir::check::check_body`: inference, borrow, match and unsafety checks) and its code
   identity (`hir_ty_share::code::of_item`); then, with a `Codegen`, codegen's walk from every
   function of the crate (`HirFrontend::every_function`), describing each reached instance
   with `Frontend::describe` as `Build::reach` does (its code hash, arguments, references),
   without walking into a dependency's instances, which no edit of the crate reaches. The
   crate-wide lints of E10 are outside the database and not asked.
3. **Cold**, then a **warm** pass in a new revision (`synthetic_write`) logged: `W` is every key
   executed or validated in it. A key executed in the warm pass is a query that re-executes in
   every revision whatever changed, reported as a leak of its own (`Volatile`).
4. **Per edit.** For each function `F` with a body written in a file: insert
   `let _firewall_edit: u32 = 0;` after the body's `{` (`EDIT`: it changes `F`'s MIR and code
   hash, needs no item of any crate, and changes no fact of section 2: no binding is held
   across an `.await`, the value of a `const fn` stays). Run the requests, `F`'s first, logged.
   Each executed key is classified ([`keys.rs`](../../../../../crates/hir-firewall/src/keys.rs)):
   - `F`'s own: keyed by `F`'s id, or by something of `F`'s body: a closure, coroutine,
     opaque type or anonymous constant whose owner is `F` (decoded from its interned
     location), a block, block module or nested function whose syntax is inside `F`'s, a
     macro call `F`'s request interned first; a query's interned arguments are followed
     through the ids their fields name (`Zalsa::ids_of`);
   - a **firewall**: keyed by the edited file or its text (`EditionedFileId`, `FileText`): a
     query that must execute again on any edit of the file's text and backdates when what it
     gives is equal (the parse, the `AstIdMap`, the item tree, the line index);
   - **new**: a key not in `W`, work `F`'s new body asks for (every chain of executions starts
     at a request, which is in `W`, so a leak always shows as at least one key in `W`);
   - an **exception** of section 2 that applies to `F` (E4: a constant's evaluation, when `F`
     is a `const fn`);
   - else a **leak**: an existing key of another item executed again, reported with its query,
     key, owner and the request it ran under.

   Codegen: every instance's `InstanceKey` before and after; one that changed, appeared or
   went must be `F`'s (`HirInstance::of_body(db, F)`: an instance of `F` or of its closures
   and coroutines, or one whose type arguments name them, E3 and E6). Every other body's code
   hash must be unchanged, and `F`'s must have changed.
5. **Undo.** The edit is undone and checked the same way (an undo is a body edit too), and `W`
   grows by what the two passes executed.

What it asserts per edit is exactly the brief's set: the queries executed again are `F`'s plus
the firewalls and the exceptions that apply, and the code compiled again is `F`'s.

### Real crates

`tests/real.rs` reads one JSON value in `BLAZE_FIREWALL` (refusing unknown fields): the
harness projects (`harness/apps/*`, and `harness/apps/apps.json`'s repositories checked out),
each built once with `blaze build` so its build scripts have run, its registry packages in
`cargo_home`. It lowers each with `load_cargo::v2::lower` (the build's own lowering), applies
the runs and proc macros as the daemon does, and runs the harness over the member crates' every
function, or a stride of them. It is `#[ignore]`d: the harness runs it with `--ignored`.

### What it found, and the fixes

Nothing has been built yet (no lane runs anything before the first compile). Predicted from
reading the code, and to be confirmed or refuted by the first run:

- `body_syntax(G)` (`hir-def/src/expr_store/body.rs`) is keyed by each body of the edited file
  and reads the file's parse and `AstIdMap`, so it executes again for every body of the file and
  backdates (its `PartialEq` leaves out the text and position). Per the brief this is a leak of
  cost O(bodies in the file) per edit; it is listed under its own name, so the first run says
  whether anything past it executes. The fix at the cause is a per-file query that gives each
  item's position-free syntax hash as a field of a tracked struct per item (salsa reads one
  field), which `body_syntax` reads instead of the parse, as the item tree's per-item keys
  already do (`item_tree/items.rs`, [`../frontend/reparse.md`](../frontend/reparse.md)).
  Queued to the format lane (request 00845), which owns `expr_store/`.
- A query taking an `ExpressionStoreOwnerId` among several arguments (`closure_exprs`) has
  interned arguments that name no id, as the type is no salsa supertype, so the harness can't
  tell it is `F`'s and would report it. The fix is `NamesIds` for `ExpressionStoreOwnerId`
  in `hir-def`, queued to the db lane (request 00846).

## 4. Prior art

rustc's red-green graph has the same shape: `typeck(F)` reads `hir_owner_nodes(F)`, each HIR
owner hashed, so an edit of `F` re-checks `F` and validates the rest; but its crate-wide
`crate_inherent_impls`, `trait_impls_of` and resolution make some edits reach every body, and
its CGUs recompile every function sharing one with `F`. Roslyn rebinds only a method whose text
changed. The harness is the check neither has: it asserts the property per function over real
crates, the way `rustc`'s incremental tests (`tests/incremental/`, `#[rustc_clean]`) do per
hand-written case.
