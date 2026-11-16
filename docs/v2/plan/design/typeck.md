---
status: plan
summary: The per-body type-check driver written new (crates/blaze-typeck): one body in, one record out, a pure function that returns its result or the facts it lacks; every fact from outside the body read through one sealed recorder, so a result cannot depend on what its read list does not hold; the walk an explicit work stack in rustc's order; results and diagnostics in portable columns. The type tables, the next-generation trait solver, MIR building and the borrow checker are kept; the old driver (hir-ty's infer and method_resolution) is the referee until every body of core, alloc and std agrees.
date: 2026-11-15
---
# The type-check driver, from scratch

## For a successor (2026-11-16)

The state of the real type context. This section is the lane's handoff; delete it when its open
list is done.

**Where each piece is** (`crates/blaze-typeck/src/`):
- `infer/terms.rs`: `Terms`, blaze-solve's `Tys` over blaze-ty's arena (`TyCx`).
  - A solver `Ty` or `List` is a blaze-ty node's bits.
  - A solver `Def(u128)` is the low 16 bytes of an `ItemKey` (`item_key`, `def_of`).
  - A solver variable is the blaze-ty index times 8, plus a kind code (`var_index`).
  - Regions are erased. `Lazy` relation is `cx.eq`; `Structural` is `unify_structurally`.
  - Emitted projection goals come back as `deferred` pairs.
- `infer/facts.rs`: `FactCache`, blaze-solve's `Facts`, filled only through `Reads` (the one seam).
  - On a solver `Stop::Need(q)`, `fill(q, reads)` reads the question, converts it to the solver's
    portable form, and the solve runs again (`InferCtx::need_rounds` counts these).
  - An associated type's solver def is `assoc_def(trait, name)`, a 128-bit hash.
  - `names` maps it back.
- `infer/mod.rs`: `InferCtx`, which implements `TypeCtx`.
  - Obligations live in the context's columns (`obs`, `preds`, `state`, plus an undo log), so a
    snapshot covers them.
  - Each `select` hands the pending goals to a fresh `Fulfill<Wake>`; the solver's cache carries
    proofs across calls.
  - The param env is interned lazily from `assume`.
- `instance.rs`: `select_impl` and `resolve_instance` for monomorphization, through the same
  solver.
  - Candidates come from the self shape's bucket and the `Shape::Param` bucket.
  - Each candidate's trait reference is unified with the goal's and its where-clauses proved.
  - Two survivors is `Overlap`. A world miss is `Missing(question)`.
- `asks.rs`, plus `Reads::logged_in` and `check_logged`: the ask log, and `restarts(log, order)`
  for R under the `Pull`, `Kind` and `KindWithGoals` orders.
- `toy.rs`: only the tests' `ToyWorld`, a test-only module. `ToyCtx` and the `toy` feature were
  deleted on 2026-11-16, after blaze-ladder switched to `InferCtx`. Everything now checks with
  the real context.

**The seam:** every read of another item's record goes through `Reads::record` (world.rs); the
`FactCache` never sees the `World` directly.

**Snapshot contract** (blaze-ty `VarTable::rollback_to`, and blaze-solve `tys.rs`):
- `commit` closes the latest snapshot only.
- `rollback_to` may name an older snapshot and closes every snapshot opened after it. The
  solver's stop-unwind relies on this.

**Tests** (run with `bash /Volumes/Projects/blaze-int-1112-notes/lanecheck.sh <crate> test
[filter]`; never cargo):
- blaze-typeck: the drawn-body properties now run on `InferCtx`. They passed byte-equal against
  the toy before it was deleted. These bodies select no impls, so they prove the plumbing, not
  the solver.
- blaze-ladder, `every_rung_checks_with_the_real_context`: rungs 1 to 7. 7a and 7b join when they
  lower.
- blaze-ladder, `the_real_context_refuses_an_unmet_bound`: the control.
- blaze-ladder, `rung_7_resolves_each_impl_method`.
- blaze-ty, `rollback_to_an_outer_snapshot_closes_the_inner`.
- The last green lines (2026-11-16, late): "test blaze-typeck: exit 0; 36 passed, 0 failed",
  "test blaze-solve: exit 0; 5 passed, 0 failed", blaze-lower 44 passed with one failure that
  is not the checker's (`expand_tests::a_derive_clone_is_an_impl_lowered_with_its_item`).
- Body suites: `src/suite/` (Bb-built bodies, each with every expression's expected type and a
  control) and `crates/blaze-lower/src/typeck_tests.rs` (bodies from text through `to_typeck`;
  lanecheck hides numbered lines, so to see a body's typed lines, write them to a scratch file
  once, pin, remove the write).

**Done on 2026-11-16 (late):** fn-pointer headers (`FnPtrHeader`: ABI and safety in
`PNode`/`TyView`/solver `TyKind::FnPtr`; `FnSig::header`; reify keeps the header;
`AdjustKind::UnsafeFnPointer`; no Fn* impl for an unsafe fn pointer; suite `fn_ptr`).
`format_args!` and `.await` in async fns (rules/async_fmt.rs; `async_sites` in machine.rs; suite
`async_fmt`). `where const` (written by the extensions lane; suite `where_const`). The census'
two wrong answers (op.rs `binop` no longer needs the left type at once; pat.rs byte string
against `&[_]`), tests in blaze-lower `typeck_tests`.

**Who holds what (2026-11-16, coordinator):** the async lane (acb7071a95dc651c4) holds
rules/mod.rs, closure.rs, the coroutine kind in ty.rs and portable.rs, blaze-ty, and blaze-solve
builtin.rs for `Future`. The forms lane (a140bd4628d7499e9) holds pat.rs, op.rs, block.rs,
lit_path.rs and instance.rs. The owner of the crate keeps the rest and the last word on design.

**Open, in this order:**
1. `dyn`: no Dynamic `PNode`/`TyView`, unsizing is only `[T; N]` to `[T]`; needed for core's
   `fmt::write(&mut dyn Write, ..)` (the hosted hello's failing-`Result` main) and every
   `&dyn Tr`. Needs the solver's object candidates.
2. Overloaded operators record no callee row (promised to blaze-consteval).
3. Speed: the census profile (section "Checker at scale"): writeback's export folds a full
   resolve per node (26% of self time), clock reads 10%, relate/generalize 9%; 117 to 304x
   the floor. The cost example said 1544x.
4. blaze-impls: a 16-byte fingerprint per impl per (trait, shape) bucket, with a test and a form
   bump. Then fill `ImplSet::columns` in `FactCache` (today `Columns::EMPTY`, so every impl is a
   candidate).
5. `vtable(world, trait_, self_ty)` for rung 7b, once 7b lowers. Rung 7c (`From<T>`,
   `FromResidual`, `Try`) comes through the existing `TraitDef` and `ImplHeader`. If 7c needs an
   associated type's own bounds, add `bounds: &[PClause]` to `AssocTyDef`; the adapter passes 0
   today.
6. The discrimination tree, last, on real impl records.

**Where the semantics are least sure to match Rust:**
- **Ambiguous goals:** `select_all` turns anything still pending or overflowed into `Ambiguous`.
  The real solver's `Maybe` with a cause is not mapped to rustc's ambiguity errors.
- **Projection errors:** `select` reports a failed projection goal as `Mismatch` and any other
  failure as `Unsatisfied`, coarser than rustc's error kinds.
- **`unify`:** projection pairs from a lazy relation are registered as obligations, with a
  placeholder `Goal::WellFormed(alias)` in the obligation column, so an error report on one names
  the alias, not the projection.
- **Closures:**
  - The closure's def is a hash of its `ExprId`, not of the body.
  - The closure kind is a fresh const variable that nothing yet constrains.
  - Captures are not modelled in the solver's view.
- **Opaque types:** generics are reported as 0, so `impl Trait` in a signature is not
  instantiated.
- **Trait and impl records:**
  - `where_clauses` and `item_bounds` on a trait are empty; supertraits only.
  - An associated type's own generics (GATs) are 0.
  - `ImplHeader` polarity is always positive. Negative impls are not read.
- **`may_hold`:** it treats the solver's `Maybe` as may-hold, which is rustc's
  `predicate_may_hold`.
- **Integer and float variables against the error type:** they stay unbound, while a type variable
  binds. This matches rustc's `TypeRelating`; the toy was changed to agree.

The owner, 2026-11-15: "feel free to remake type checker from scratch in ideal one ... our best
design from scratch will be better as fewer incremental issues". Narrowed by the main agent: keep
the type tables, the Rust project's next-generation trait solver (`vendor/rustc_next_trait_solver`
through `rustc_type_ir`'s `Interner`), MIR building and the borrow checker; write new the
per-body driver: the walk over a body's expressions and patterns, expectations and coercions,
method and path resolution inside bodies, closures and captures, and the obligations it
registers. That is where incremental bugs live: a result that depended on something it did not
record.

The crate is `crates/blaze-typeck` (the name `blaze-check` is the model checker's). It sits beside
`crates/hir-ty/src/infer.rs`, `infer/` and `method_resolution.rs` (about 41k lines), which stay as
the referee: for every body of real `core`, `alloc` and `std` both give the same types before
the new one replaces the old.

## What it is judged by

1. **No unrecorded read.** A body's record is valid while every entry of its read list still sees
   the answer it saw (bottom.md, read-addressed.md). If the driver can learn a fact without
   recording it, a stale record is served after an edit. So learning without recording must be
   impossible to write, not merely forbidden.
2. **One pass, usually.** The record lists what the check needed; the next check of the body gets
   those up front and runs once with no re-entry.
3. **Depth costs nothing.** A body of 100,000 nested operators checks in the same frame of native
   stack as `1 + 1`.
4. **Equal to rustc.** rustc's order of inference is the specification: where expectations flow,
   where a type must be resolved structurally, when pending obligations are selected, fallback,
   the adjustments, the order of probe steps. Each rule file names the rustc file it follows.

## The function

```rust
pub fn check<W: World, C: TypeCtx, A: Allocator + Clone>(
    body: &BodyView<'_>, world: &W, ty: &mut C, hints: &Needs<A>, alloc: A,
) -> Result<Outcome<A>, CheckError>;

pub enum Outcome<A> { Done(Checked<A>), Need(Needs<A>) }
```

- A pure function of the body, the world's answers and the hints. No global state, no locks, no
  thread-locals, no interior mutability shared across bodies. Two runs with equal inputs give
  equal bytes (rule 25), whatever the type context numbered its variables.
- `Need(questions)`: the world had no answer yet for a fact (a callee's signature not yet made, an
  impl bucket not yet fetched). The caller gets those records (reading them, or running their
  tasks) and calls `check` again from the top. Nothing of the run is kept: re-entry is cheap
  because what was read is a record, and the state needed no stack to be saved.
- `hints` is the previous record's `needs`: the caller gives it to the world's `prefetch` before
  the walk starts, so the usual run has every answer.
- `CheckError` is the driver's own failure (a bound exceeded, malformed input from the body's
  lowering or the world). A user's type error is never a `CheckError`: it is a value in the
  record (below).

## `World`: the only way out of the body

`World` is the one interface through which the driver learns anything not in the body:

| Question | Answer |
|---|---|
| `FnSig(item)` | generics (parent count, own count), inputs, output, as portable types |
| `Predicates(item)` | the where-clauses of an item, own and inherited |
| `Adt(item)` | kind, generics count, variants with ctor kind and fields (name, type) |
| `ImplsOf(trait, shape)` | the impls of a trait whose self type can have that shape, blanket ones included |
| `ImplHeader(impl)` | generics, self type, trait arguments, associated type bindings |
| `Inherent(shape, name)` | inherent associated items of that name for a self shape |
| `AssocItem(container, name)` | a trait's or impl's associated item of that name |
| `TraitsInScope(scope)` | the traits in scope at a block of the body (imports, preludes) |
| `Path(scope, path)` | a value or type path's resolution, and how many segments it consumed |
| `Lang(item)` | a lang item |
| `Const(item)` | a const's value, where typing needs it (array lengths) |
| `Opaque(item)` | an opaque type's hidden type |
| `Attrs(item)` | attributes that change typing (`non_exhaustive`, `fundamental`, `rustc_*` kinds) |
| `TraitDef(trait)` | generics, flags (auto, marker, unsafe, fundamental, coinductive, const, dyn-compatible), supertraits, associated types with defaults |
| `AssocParent(item)` | an associated item's trait or impl, name and kind |

**Recording cannot be skipped.** Every method of `World` takes a `Seal` as its first argument.
`Seal` has a private field and is made only inside `world.rs`, by `Reads`, the wrapper that owns
the read list. An implementation can write its methods (it names the type) and cannot call them
(it cannot make one); the rule modules cannot make one either, since its constructor is private
to the module that records. `Reads::ask` calls the world, hashes the answer with the answer
type's own `ContentHash` (negative answers hash too: "no impl of T for this shape" is a read),
appends `(question, hash)` to the read list, and returns the answer. A body's rules see only
`&mut Reads`. The test build has one backdoor (`Reads::peek_unrecorded`, `cfg(test)`), used by
a planted bug to show that the read-list oracle catches a read the type system would refuse.

The solver reads through the same wrapper: `TypeCtx::select_where_possible` takes `&mut Reads`,
so impl lookups made while selecting are reads of the body.

A world answers `Missing` for a fact it lacks; `Reads` turns that into `Stop::Need(question)`,
which every rule propagates with `?` to `check`, which returns `Outcome::Need`.

### Item records and body records

Every question reads one of two kinds of record (`Question::record_kind`):

| Kind | Questions | Made by |
|---|---|---|
| Item | `FnSig`, `Predicates`, `Adt`, `ImplsOf`, `ImplHeader`, `Inherent`, `AssocItem`, `TraitsInScope`, `Path`, `Lang`, `Attrs`, `TraitDef`, `AssocParent` | the item layer, from items' text alone |
| Body | `Opaque` (a hidden type), `Const` (a const's value; a discriminant is one) | another body's check |

**Re-entry from the top, made rare by order.** A body that misses a record stops and runs again
from the top once the driver has fetched it; nothing parks (bottom.md section 2, principle 2: a
million lines is about 70,000 bodies, and parked bodies would hold gigabytes). The order that
makes this rare is a gate: a body's check is queued only after the item records its scope can
name have settled, so its `Need` is only ever a body record. `RecordKind` is what the driver
reads to tell which questions a body may still wait on: `Body` ones, and none other once the
gate exists.

**Until the gate exists**, a body with no record (empty hints) runs a syntactic prefetch before
its walk (`Reads::prefetch_syntactic`): every path its text names, then, for the paths the world
already resolves, the signatures, ADTs, predicates and trait definitions they name, and for every
method name the associated item and trait definition of each trait in scope. These are
prefetches, not reads: nothing enters the read list, and no answer changes the run. A re-entered
body's hints are the questions asked before the stop and the one missed (`Reads::finish_need`).

### Re-entry, and what decides saved state

`check_logged` writes the ask log beside the read list: for each ask, the question, its record
kind, the walk's step and the caller's clock (rule 75: a `Clock` passed in), the checker's state
in bytes, whether it was missing, and whether it came from a ground solver goal. From one full
run's log, `restarts(log, order)` simulates a cold body: each run stops at the first record not
yet fetched and repeats the work before it. It reports, per body, entries, wasted and total
steps and nanoseconds, and serial rounds, under three orders: `Pull` (nothing there), `Kind`
(item records settled first) and `KindWithGoals` (a ground goal's miss is its own goal task,
solve.md "Goal tasks"). R is wasted over total work. **Threshold: re-entry stays where R is at
most 0.10 (`R_THRESHOLD`); where it is more even with goal tasks, those bodies alone save their
state, as the explicit work stack and the solver's explicit stacks already hold it, never as
fibers.** The property `the_ask_log_predicts_reentry` shows the simulation equals real re-entry
on the toy world (a cold body runs exactly the predicted number of times), with a planted lost
fetch as its control.

## The walk: a work stack in rustc's order

The body arrives flat (`BodyView`): an expression array, a pattern array, statements, and side
columns for child lists, fields, arms, paths and written types, every node by a `u32` index, each
with its item-relative byte range (rule 71). Locals are resolved by the body's lowering
(`Expr::Local(pat)`, as rustc's resolver does before typeck); every other path is resolved by the
driver through `World` (type-relative paths, `Type::item`, are typeck's, as in rustc).

The machine is `Machine` (machine.rs): a stack of `Frame { node, state, expect, then, saved, a, b, c, t, u, l }`.
`node` is an expression, a pattern, a statement or a block; `state` is the step of that node's
rule (a `u8`); `expect` its `Expectation` (`None`, `HasType(t)`, `Castable(t)`,
`RvalueLikeUnsized(t)`, as rustc's); `then` is where the result goes: nothing, a coercion to a
type, an equality demand, or a `CoerceMany` site; `saved` holds the divergence flag rustc saves
around each expression; `a`, `b`, `c` (`u32`), `t`, `u` (`Ty`) and `l` (`TyList`) are the rule's
own registers between states (an arm index, a coercion site, a scrutinee type, the formals). A rule never calls the checker of a child: it pushes itself back with
the next state and then the child, and the child's type comes back in one register. No native
recursion follows the expression tree, so depth is bounded by the frame column, sized from the
body (`4 * nodes + 64`), whose overflow is a typed `CheckError`, not a crash.

Deferred work sits in typed queues and is processed where rustc processes it:

- `casts`: cast checks, after fallback (`check_casts`).
- `numeric`: every integer and float variable the driver made, for fallback to `i32` and `f64`.
- `diverging`: variables that received a `!` by coercion, for never-type fallback (`!` in every
  edition on the pinned rustc 1.100, as Relax's port of fallback.rs @8d1a764 has it), which
  runs before the integer and float fallback.

A probe or a failed `commit_if` rolls back the machine, not only the type context: the
diagnostics and error count, the adjustments, callees, fields and captures written, and every
queue above, by length; the type context rolls back its variables and pending obligations. The
read list is kept: a failed probe's outcome was decided by what it read.
- `closures`: closures whose capture analysis runs after selection (`closure_analyze`), and
  calls of a closure whose kind was not known yet (rustc's deferred call resolutions).
- `coerce_many`: the state of each `if`/`match`/loop/block-break/return site.

The order at the end of a body is rustc's `typeck_with_inspect`: select where possible; fallback;
casts; select where possible; closure analysis; select all or error (ambiguity reported as
"type annotations needed"); writeback.

A method call on a receiver whose type is still a variable is not deferred: rustc selects pending
obligations, resolves the receiver structurally and reports E0282 if it is still unknown; that
is the specification, so the driver does the same.

All state is plain data in columns of `u32` indices and `Ty` handles, so a run can stop at
`Need` anywhere and, later, be saved and resumed if a profile says re-entry costs.

## Results: one record in columns

`Checked<A>` holds, all `Portable` (items by `ItemKey`, types in the record's own type table):

- `types`: a `PortableTys` table, hash-consed by structure, so equal types are equal indices and
  the bytes depend on structure and walk order only, never on a context's handles;
- `expr_ty` and `pat_ty`: one `PTy` per expression and pattern; `local_ty` per binding;
Sparse facts are `blaze_own::Soa` tables sorted by expression, so a lookup scans one column:

- `adjusts`: per expression, consecutive `Adjust` steps (`NeverToAny`, `Deref(builtin or
  overloaded)`, `Borrow(mutbl, two_phase)`, `ReifyFnPointer`, `Unsize`), each with its target;
- `callees`: per call and method call, the item and its generic arguments;
- `fields`: per field access, the variant and field index (tuples too);
- `captures`: per closure, the captured places and how (by ref, by mut ref, by value);
- `diagnostics`: typed `Diag { code, at: Range, .. }`, item-relative;
- `reads` and `needs`: the read list and the questions to fetch next time.

## Diagnostics are values

The driver never panics on a user's error and never stops for one. A type error records a
`Diag` and gives its expression the error type, which unifies with everything and silences
errors that follow from it, as rustc's `ty::Error` does. Codes are rustc's (`E0308`, `E0282`,
`E0599`, `E0609`, `E0061`, `E0614`, `E0606`, ...), so a person's search finds the same page.

## `TypeCtx`: what is asked of blaze-ty

Until `crates/blaze-ty` exists (another lane extracts the type tables and the solver adapter
from hir-ty), the crate defines the traits it needs and implements them in tests over a toy
type language. Each item says what it becomes:

- `Ty`, `TyList`: 4-byte handles; become blaze-ty's `Ty` and interned `GenericArgs`.
- `TyView`: a copyable view of one level of a type (`Bool`, `Char`, `Int`, `Uint`, `Float`,
  `Str`, `Never`, `Tuple`, `Ref`, `Ptr`, `Array`, `Slice`, `Adt`, `FnDef`, `FnPtr`, `Closure`,
  `Param`, `Alias`, `Infer`, `Error`); becomes `rustc_type_ir::TyKind` through the interner,
  regions erased as rustc's typeck sees them.
- `mk`, `view`, `mk_list`, `list_get`: the interner.
- `fresh(kind)`, `shallow_resolve`, `resolve`, `unify`: the `InferCtxt` table operations.
- `snapshot`, `rollback_to`, `commit`: `InferCtxt::probe` and `commit_if_ok`; the driver wraps
  them in `core::mem::DropGuard` so an early `?` rolls back.
- `assume(clause)`: the body's param env.
- `register(obligation)`, `select_where_possible(reads)`, `select_all(reads)`, `next_error`:
  the next solver's `FulfillmentCtxt`, its interner reading impls through `Reads`.
- `may_hold(obligation, reads)`: `predicate_may_hold` in a probe, for the method probe.
- `normalize(trait, self, args, name, reads)`: an alias normalized now if selectable, for
  overloaded deref and operator outputs.
- `closure_hint(var, [Fn, FnMut, FnOnce], Output)`: the inputs of the first pending
  `var: Fn*<(..)>` obligation and the term of a pending `Output` projection, for a closure's
  expected signature (`deduce_closure_signature`).

From blaze-ty the driver also needs the **portable type form** (`PNode`/`PTy` here): a table of
nodes with children before parents, items by key, generic parameters by index, and a hole
(`Infer`) only in a body's written types; and a content hash of it (`ContentHash` here, FNV-1a
until blaze-ids' `ContentId` is at zero third-party dependencies).

## Rules, by file

Each file under `src/rules/` begins with the rustc file it follows (and the Relax file that maps
it). Built first: literals and paths (with turbofish and type-relative paths), blocks, `let` and
statements, calls of fn items and fn pointers (arguments in rustc's two passes, closures last
after a selection), method calls (the probe: steps by autoderef with the array-unsize step last;
per step by value, then `&`, then `&mut`; inherent before traits in scope), fields and tuple
indexing through autoderef, operators (the builtin path for scalars before the trait), borrows
and dereference, casts (deferred), `if`/`match`/`loop`/`break`/`return` with divergence and the
never type, patterns with default binding modes (and edition 2024's refusals), struct, tuple
and array expressions, coercions (`Coerce::coerce`'s order: unsize, then by target pointer,
then by source fn item or pointer, then unify; `CoerceMany`), integer, float and never
fallback, and writeback.

Later, in this order: closures' expected-signature deduction from pending `Fn*` obligations and
capture analysis as its own pass; associated items and projections through the solver;
`impl Trait` in argument and return position (opaques' hidden types recorded); unsized types and
well-formedness obligations; `?` and the other desugarings as the body's lowering gives them.

## Proof

Through `blaze_sim::check`, on generated bodies over a generated world:

- **Determinism**: two runs of one body over one world, each with a fresh type context, give
  equal records.
- **The read list is complete**: change an answer of the world that the read list does not hold,
  and the record is unchanged; change one it holds, and the read list's hash for it differs. A
  planted bug (a field lookup through `peek_unrecorded`) must be caught (`blaze_sim::control`).
- **Re-entry equals one run**: a world that answers `Missing` until asked, driven by `Need` until
  `Done`, gives the record of a single run with every answer present.
- **Depth**: 100,000 nested parentheses and 100,000 nested binary operators check on the test
  thread's stack.

The referee test (both drivers agree on every body of `core`, `alloc`, `std`) comes when
blaze-ty and the new item layer give both drivers the same bodies.

## Floors (rule 67)

A body's check is linear in its nodes plus the solver's work: one frame push and pop per node,
one column write per result, no allocation per node (every column is one buffer from the task's
allocator), and the reads it makes. The floor for a repeat check with nothing changed is zero:
the record's read list stands and the driver does not run. The ratio is measured on std's
bodies once blaze-ty lands.

## Owed to blaze-lower

blaze-lower (crates/blaze-lower) lowers bodies with rustc's desugarings and converts them to this
crate's `BodyView` in `blaze_lower::typeck::to_typeck`. It refuses each form below by name
(`Refused::Form`), because `BodyView` has no place for it. In order of how much code needs it
(numbers kept as the forms land):

1. Taken (2026-11-16). A desugaring's lang-item path is a `BodyPath` with `lang` set and no
   segments (rustc's `QPath::LangItem`), resolved by `World::lang_path` to a `Def`: a fn, a
   variant's ctor or a struct (`Question::LangPath`, an item record). A tuple variant's key
   answers `fn_sig` as its ctor fn. `?` checks through `Try::branch` and
   `FromResidual::from_residual` like any trait call (suite `lower_forms`; blaze-lower's
   `typeck_tests` checks rung 7c's `main` from its text). `for` over a range and over a slice
checks the same way, through `IntoIterator::into_iter`, `Iterator::next`, `Some`, `None` and
`Range` (`typeck_tests::for_over_a_range_and_a_slice`, world `fixture::loops`). blaze-lower's `LangItem` and this
   crate's are two enums mapped one to one in `to_typeck`: lower's is a 64-bit set in its record,
   this one has more than 64 items; merging them is owed.
2. Taken (2026-11-16). `let` in a condition (`if let`, `while let`, let chains) is
   `Expr::Let { pat, init }`, as rustc keeps it: `bool`, the pattern checked against a fresh
   binding type `init` coerces to, as a `let` statement's (`expr_let`; test
   `typeck_tests::let_in_a_condition_and_let_else`). blaze-build's walk refuses it by name until
   it builds the test; blaze-consteval refuses it in a constant.
3. **Breaks with a target that is a desugaring's**: `?` in a `try` block, the `break` a `for`
   or `while` makes; and loops by source (`for`, `while`), carried for diagnostics.
4. **Coroutines**: async fns and `.await` taken (2026-11-16): `BodyView::coroutine` is
   `Some(CoroutineKind::Async { output })` for an async fn's body, checked against the written
   `output`; `Expr::Await(e)` proves `T: IntoFuture`, is `<T as IntoFuture>::Output`, records
   the `into_future` callee, and is E0728 outside an async body (a closure in one included).
   The signature's output, `impl Future<Output = T>` as rustc lowers it, is the signature side's
   (the ladder front). Owed: `Expr::AsyncBlock` (refused, `CheckError::Unported`) needs a
   coroutine type and the solver's builtin `Future` impl; async closures, `gen` blocks, `yield`.
5. Taken (2026-11-16). **Format arguments**: `Expr::FormatArgs { args, uses }`, not desugared;
   `uses` in `BodyView::fmt_uses`, one `(arg, FmtTrait)` per placeholder. Each argument is
   checked once and borrowed shared (an adjustment), each use proves `T: Trait` (`T` the
   argument's type, as `Argument::new_display<T: Display + ?Sized>`), a count use coerces the
   argument to `usize`; the node is `core::fmt::Arguments` (suite `async_fmt`).
6. **Patterns**: box, range (`a..=b`, `a..`), rest outside a tuple, a slice's `..`, macro calls.
7. **Literals**: `c"..."`. (`b'x'` is taken: a `u8` integer literal, `Lit::Int` with suffix
   `u8`, in expressions and patterns; its value is read from the text at its range.)
8. **The rest**: `become`, raw borrows (`&raw const`), `const` and `try` blocks, struct
   defaults (`..` with no base), path anchors (`<T as Tr>::x`), absolute paths, lifetime,
   const and associated-item generic arguments, inline asm, `offset_of!`, `_` expressions.

Dropped in the conversion, as no type depends on them: match and loop sources, `unsafe` on a
block, the shadowing links, and the hygiene of names made by desugarings.

blaze-lower's record keys hash the item's raw bytes (its text, item-relative), never a token
stream, the same bytes the outline's entry digests now hash. `ItemKey` is only where a record is
filed: two items of equal text share one record.

## Checker at scale (2026-11-16)

A census of real bodies, from `crates/blaze-real/examples/check.rs`. Every number below is from
one run on 2026-11-16.

**Two modes:**

- **Front mode** (`check <crate dir>...`): the ladder front gives real answers in a one-crate
  world with no core. When the front refuses an item, the census blanks it and rebuilds the front,
  then checks every body that is left.
- **Forms mode** (`check --forms <library> <dir>...`): stage h's pipeline. Each body is outlined,
  lowered with expansion and put through `to_typeck`, then checked in a world that knows no item.
  It measures how many forms reach the checker. Its E0282 and E0425 are artifacts of that empty
  world.

**Sets:**

- tempo: 48 crates, 14,816 bodies.
- bstd and the blaze crates beneath it: 32 crates, 12,801 bodies.
- core and alloc: 8,549 bodies.

**Results:**

| | tempo | bstd | core+alloc |
|---|---|---|---|
| front mode: clean | 3.7% | 7.7% | |
| forms mode: reach the checker | 86.2% | 95.3% | 94.5% |
| forms mode: stopped by names alone | 7,424 | 6,320 | 4,687 |

Of 43 right probe programs, 40 check clean. The other three are the two wrong answers below
(`|x| x + 1u32` and `|a, b| a + b` are one class). The rest of the front-mode residue is the
world: no core, prelude, globs, extern crates or `cfg`.

**Cost.**

- Forms mode, check alone on 1 thread:
  - tempo 63,237 bodies/s;
  - bstd 71,747 bodies/s;
  - core+alloc 128,327 bodies/s.
- Lower plus check: tempo 1,100 ms on 1 thread and 285 ms on 18.
- Heap peak 5.1 MB; record about 2.4 KB per body on tempo.
- Against the floor in `cost.rs`: 117 to 145x in forms mode, and 249 to 304x in front mode, where
  the checks run to the end. It was 1,544x.
- Restarts: the `Pull` order wastes R = 2.3; `Kind` wastes 0.

From a `sample` of the check on tempo, by self time:

1. `writeback`'s `export` through `TypeCtx::resolve`, a full fold for every node: 26%.
2. Clock reads for the ask log: 10%, part of it the census's own `Timed` wrapper.
3. relate, generalize and unify: 9%.

The slowest bodies are long array literals (blaze-unicode tables: 8,326 expressions in 1.8 ms).

**Lanes.** Each lane fixes its wrong answer first, with the repro as a test that states the rule.
Rerun commands, from the repo root:

- tempo directories: every directory under `/Volumes/Projects/blaze-tempo-1116/tempo` that holds
  a `Cargo.toml` and a `src/`.
- `$L` is the toolchain's `rust-src` `library` directory.
- Front mode:
  `CHECK_CAP_S=25 bash <notes>/mixcheck.sh blaze-real run check <crate dirs>`.
- Forms mode:
  `bash <notes>/mixcheck.sh blaze-real run check --forms $L <dirs>`.
- Probes: copy the probe program into the lane's suite.

**Rerun after the ladder port.** This is the census's last step: rerun front mode when the ladder
front moves onto the names library and the expanding lowering. The probes, directory lists and
baselines are kept in `/Volumes/Projects/blaze-int-1112-notes/census-1116/`; call it `$D`. `$N`
is `/Volumes/Projects/blaze-int-1112-notes`. Run from `/Volumes/Projects/blaze-fresh-1115`.

1. Front mode, tempo:
   `CHECK_CAP_S=25 EXAMPLE_TIMEOUT=1700 CHECK_SAMPLES=30 timeout 1800 bash $N/mixcheck.sh blaze-real run check $(cat $D/tempo-dirs.txt)`.
   Baseline (`front-tempo-1.txt`): "bodies: 12843; reached the checker: 547 (4.3%); clean: 479
   (3.7%)".
2. Front mode, bstd:
   `CHECK_CAP_S=240 EXAMPLE_TIMEOUT=1700 CHECK_SAMPLES=30 timeout 1800 bash $N/mixcheck.sh blaze-real run check $(sed 's#/src##g' $D/bstd-dirs.txt)`.
   Baseline (`run-bstd-1.txt`): "bodies: 13062; reached the checker: 1325 (10.1%); clean: 1010
   (7.7%)".
3. Front mode, probe: `timeout 900 bash $N/mixcheck.sh blaze-real run check $D/probe`.
   Baseline: "bodies: 60; reached the checker: 60 (100.0%); clean: 60 (100.0%)".

The port changes what `Front::new` takes, so `crates/blaze-real/examples/check.rs` (`run_crate`)
changes with it in the same batch. Report the new clean percentages and the top residue before
the fix lanes start.

Forms mode, last measured before the leftovers lane began (reaching the checker):

- bstd: 98.5%.
- core+alloc: 99.2%.
- tempo: 86.7%.
- Const generic arguments still stop 108 / 42 / 82 bodies alone (bstd / core+alloc / tempo).

1. **One world for `run` and `check`** (with the names library, a94a2fe582d116eb4, and the
   ladder port, a1e0251f85540e9c2). Do not build a second world. Make the entry-driven
   implementation one library that both the ladder and the driver's check task call. What `check`
   needs that reach-from-an-entry lacks:
   - every body asked for, including those no entry reaches;
   - `ImplsOf` and `Inherent` complete over the crate set, because a missing impl changes method
     and trait answers;
   - test targets, `tests/`, examples and `build.rs`, with `cfg(test)` set.

   Attributes on items:
   - Understood: `cfg` and `cfg_attr` (expansion), `derive`, attribute proc macros (`tokio::main`
     and `tokio::test` rewrite the body), `repr` (an enum's discriminant type), `lang`, `path`,
     `macro_export` and `macro_use`.
   - Inert for typeck: `test`, `ignore`, `should_panic`, `inline`, `cold`, `must_use`,
     `track_caller`, `doc`, the lint levels, `rustfmt::*` and `clippy::*`.

   Outcome: forms-mode names alone, 7,424 on tempo; front mode, 4,863 "names nothing" and 3,021
   attributes. Rerun the census when the port is in and publish the new residue before lanes 2-4
   start.

   The world is built (2026-11-16): `crates/blaze-world`, one function `world(graph, lock,
   toolchain, packages, checkout, scripts, alloc)` from the unit graph to `resolve_crates`'
   inputs. Each held unit (target-side lib, and every proc macro with its host-side deps not
   followed) is a crate keyed from its text and inputs; equal keys are one crate; order is the
   toolchain's, then by key, so the inputs' bytes do not depend on work order. An absent source
   is a typed error naming the package. `unit_input` is the per-unit part the driver's names
   task calls. Build-script cfgs come in as a table (`ScriptOutputs`); the caller fills it.
   On tempo (example `names`, tree 10763b356c, store s08-v3-all), 1,022 crates (16 toolchain,
   79 proc macros, 89 without their build script's outputs), 5.60 M lines read:
   - 18 threads 18.6 s, 1 thread 20.6 s; peak footprint 5.1 GB. The critical path is
     libc 0.2.190 (6.3 s; 3,552 macro paths in 57 modules) and core (1.9 s): the solver's.
     Floor (rule 67): the outline reads 11.9 M lines in 1.0 s on 18 threads, so 5.6 M lines
     is 0.47 s: the ratio is 40x.
   - Unresolved: 10,306 imports of 136,819 and 5,115 macro paths of 85,099. The largest cause is
     that no proc macro is declared: blaze-scope never makes `MacroKind::ProcMacro` from a
     proc-macro crate's `#[proc_macro*]` functions. That covers 3,967 derives, 833 attributes,
     55 bang calls and 73 re-exported proc macros, plus every item they hide: 1,932 imports
     under an attribute proc macro, and modules inside `commonware_macros::stability_scope!`.
     The next causes are 3,716 imports inside proc-macro crates (host-side deps not held) and
     build-script cfgs not yet joined (rustix's `backend` is chosen by `cfg_attr(libc, path)`).
     Sent to the names lane: proc-macro definitions, `local_inner_macros`, `$crate` from another
     crate, and builtin derives inside a macro's output.
2. **Async** (with the expansion lane, a0ac97cbffdc16b38, which owns the lowering).
   - Owns: blaze-typeck `rules/mod.rs` (`AsyncBlock`, now `Unported`), `rules/closure.rs` (async
     closures), the coroutine kind in `ty.rs` and `portable.rs`, blaze-ty `kind.rs`, `tys.rs` and
     `fold.rs`, and blaze-solve `builtin.rs` (the builtin `Future`).
   - Outcome on tempo: async blocks 528, async closures 9, async fn (front) 1,031.
3. **Paths and generics.**
   - Owns: blaze-lower `typeck.rs` (the path and generic-argument conversions only) and
     blaze-typeck `body.rs` (`BodyPath`), `rules/lit_path.rs` and `instance.rs`.
   - Covers: `<T as Tr>::x` anchors, `::x`, const and lifetime arguments, const items and const
     parameters. The latter two are the front side, to agree with lane 1.
   - Outcome, alone (tempo / bstd / core+alloc): 165 / 184 / 214. In front mode: const items
     44 / 227 and const parameters 17 / 272.
4. **Expressions and patterns.**
   - Owns: blaze-lower `typeck.rs` (expression and pattern conversions; it shares the file with
     lane 3, split by function) and blaze-typeck `rules/pat.rs`, `rules/op.rs` and
     `rules/block.rs`.
   - First, the two wrong answers, then the forms.
   - Wrong answer 1: `let add = |a, b| a + b; add(1u32, 2u32)` gives E0369.
     - `op.rs:191` takes the trait path only when the left type is known.
     - rustc instead registers `?A: Add<?B>` as ambiguous and projects `Output`, so the closure's
       parameter types can come from its call.
   - Wrong answer 2: `match dir { b"/" => .. }` with `dir: &[u8]` gives E0308.
     - A byte-string pattern is `&[u8]` when the scrutinee is a slice.
   - Forms: `_` expressions, raw borrows, range patterns, slice `..`, try and const blocks,
     destructuring assignment and `c".."`. `impl Trait` in argument and return position is the
     front side, with lane 1.
   - Outcome, alone: 7 / 377 / 237. In front mode: `impl Trait` 62 / 93.
5. **Speed, last.**
   - Owns: `rules/writeback.rs`, `infer/mod.rs` and `machine.rs`.
   - One cheap cause: skip `resolve` for a type with no inference variable, which needs a flag on
     the interned type. That flag is in blaze-ty `tys.rs`, which lane 2 owns, so this lane goes
     after it.
   - Next, one clock read per run instead of one per ask.
   - Outcome: the floor ratio on tempo forms mode, from 145x, reported with before and after.

Not anyone's lane: `cfg` in bodies and macros from dependencies (`info!`, `bail!`, `json!`,
`try_join!`; 1,862 tempo bodies) belong to the expansion lane. The census harness passes only the
workspace's and the standard library's macro texts.

### Async: decided by lane 2 (2026-11-16)

Rules from the Reference (async blocks, `await` expressions, async functions, closures).

1. **Shapes.** `.await` is one node, `Expr::Await`; the `yield` loop belongs to code generation. An async block is `Expr::AsyncBlock { body, capture }`, checked in place as its own return site and async site. An async closure is `Expr::Closure` with `is_async`. An `async fn`'s body is `coroutine: Async { output }`.
2. **Types.** An async block's type is `TyView::Future(e, output)`: one per block, keyed by its `ExprId` (item-relative) in the owner's key. In blaze-ty and the solver it is `Coroutine(def, [output, witness])`. The builtin impl is `Future<Output = output>`. `e.await` is `<E as IntoFuture>::Output`. `?` and `return` inside the block target the block's own output. An async closure returns `Future(closure, output)` per call.
3. **Auto traits.** The rule: a future is `Send`/`Sync` when its captures and every value live across an `.await` are. Captures are counted by capture kind (`&T`, `&mut T` or `T`). It is never `Unpin`, being an immovable coroutine. Liveness is not built yet. The witness is a conservative superset that can only refuse more:
   - every binding in the block;
   - every expression's type in a statement or tail that contains an `.await`;
   - every expression in a `let` initializer, since a temporary may be extended.

   The witness is a variable until the end sequence sets it, so an auto-trait goal on it waits. Owed: the exact liveness-based witness.
4. **`async fn` in the front.** Its signature's output is an opaque `impl Future<Output = T>` that captures every generic and lifetime. It waits on return-position `impl Trait` in the front (lanes 1 and 4).

## Const generics (2026-11-16, spec)

Refused today as "a const argument" in `to_typeck`: 108 bodies alone on bstd, 42 on core+alloc,
82 on tempo (forms census, 2026-11-16), e.g. `Simd::<u32, 4>::from_slice`, `first_chunk::<4>()`,
`Uint::<256, 4>::from`.

**Design.** A typeck `Ty` handle is already any blaze-ty `Arg`, not only a type:
`InferCtx::list_get` returns the arg's bits, and a closure's kind is a const variable in its
argument list. So const terms join the type forms rather than getting a second handle:

- `TyView` and `PNode` gain `ConstValue { ty: Ty, bits: u128 }` (a value of an integer, `bool`
  or `char` type, little-endian bits as consteval reads them back), `ConstParam(u32)` (a const
  generic parameter by its index in the item's generics, as `Param`), and in `TyView` only
  `ConstInfer(Var)` with a new `VarKind::Const`.
- `Array(Ty, Ty)` replaces `Array(Ty, u64)` in both; the length is a const term. `TyView`'s
  conversions in infer/mod.rs map them to blaze-ty's `const_value`, `const_param`,
  `next_const_var`, `array` (the `view` arm that panics on a non-value length goes).
- Unification of two const terms is blaze-ty's; a mismatch is E0308 as for types.
- The lowering (blaze-lower typeck.rs `type_args`) converts `GenericArg::Const`: a literal
  directly; a one-segment path naming a const parameter as `ConstParam`; any other expression
  (a `{ N + 1 }`, a path to a `const` item) through a new `Types` method that evaluates it with
  blaze-consteval (`eval_len` for `usize`, `value_of` otherwise, its `Needs` answered by the
  `Settle` loop as blaze-ladder run.rs `eval_consts` does). An explicit argument count counts
  const parameters with type parameters, in declaration order.
- A const parameter's type comes from the item's generics in the `World` (a new
  `GenericsRow` kind per parameter: type or const with its type); a const argument whose type
  differs is E0308 at the argument.

**Match sites** (`rg 'TyView::Array|PNode::Array'`, plus every exhaustive `PNode` match, the
16 files that name `PNode::Infer`): blaze-typeck ty.rs 1, portable.rs 1 (hash tags 22..24 for
the new nodes), machine.rs 3, instance.rs 4, fixture.rs 2, infer/mod.rs 2, infer/facts.rs 2,
rules/adt.rs 4, cast.rs 1, coerce.rs 1, field.rs 2, lit_path.rs 1, method.rs 1, pat.rs 2,
writeback.rs 2; blaze-build walk.rs 7, instance.rs 3, layout.rs 2, cabi.rs 1, read_back.rs 1,
tests (cabi 2, intrinsics 1, layout 3, read_back 1, tests.rs 3); blaze-consteval
machine.rs 1, machine/code.rs 5, fixture.rs 2; blaze-ladder front.rs 4, front2/types.rs 2,
run.rs 1; blaze-real examples check.rs, stages.rs, extensions.rs; blaze-lower tests.rs,
expand_tests.rs, typeck_tests.rs 1. About 70.

**Match sites, counted by the const-generics lane** (2026-11-16,
`rg -n 'TyView::Array|PNode::Array' crates`): 69 sites in 32 files over 5 crates (blaze-typeck 26
in 15 files, blaze-build 24 in 10, blaze-consteval 8 in 3, blaze-ladder 7 in 3, blaze-lower 1).
blaze-solve already has `TyKind::Array(Ty, Ty)` and blaze-ty `Array(Ty, Const)`: no change there.
Exhaustive `PNode`/`TyView` matches that take the new variants: 24 files (the list above plus
blaze-typeck rules/call.rs, closure.rs, fallback.rs, op.rs).

**Order of saves** (one batch, back to back, then check all at once): blaze-typeck ty.rs and
portable.rs (the variants), infer/mod.rs (conversions), machine.rs and the rules, fixture.rs and
instance.rs; then blaze-build (where a length is needed, read `ConstValue` and refuse a
`ConstParam` left after substitution as a typed error), blaze-consteval, blaze-ladder,
blaze-real examples, blaze-lower last (the `GenericArg::Const` arm and the `Types` method). Then
`lanecheck.sh` blaze-typeck, blaze-lower, blaze-ladder, blaze-build, blaze-consteval, blaze-real.

**State (2026-11-16, const-generics lane).** Steps 1-3 are saved and checked: blaze-solve's
`TyKind::ConstValue(ValTy, u128)` keeps a value's type; blaze-typeck has `TyView::{ConstValue,
ConstParam, ConstInfer}` and `PNode::{ConstValue, ConstParam}`, `VarKind::Const`, tests in
`suite/const_terms.rs` (planted `InferPlant::AnyLength`, caught by `control_any_length`); and
`Array(Ty, Ty)` / `PNode::Array(PTy, PTy)` at every site ("check ok" for blaze-typeck,
blaze-lower, blaze-consteval, blaze-build, blaze-ladder). A length reads back through
`PTys::value_u64`; `PortableTys::{usize_value, array_n}` make one. `Expr::Repeat.len` is a `PTy`
(blaze-lower still asks `Types::repeat_len` for a `u64` and interns it). A length `_` in a body
lowers to a fresh const variable (`Machine::lower_len`, and `lower_node` for `[T; _]`). An array
pattern against a length that is no value: without `..` the length unifies with the count (E0308
if it cannot), with `..` it is the new `DiagCode::ArrayLenUnknown` (E0730). blaze-build's
instance key domain is "key 2".

**Step 4, designed, not built.** (a) The World gains `const_params(item) -> ConstParams { params:
&[(index, PTy)], types }` over all the item's generics, parent first, as a `Question` of
`RecordKind::Item`; implementers: toy.rs, fixture.rs, blaze-build tests.rs, blaze-ladder front.rs
and front2 (world_items.rs, fill.rs), blaze-real check.rs (`Timed`, `Empty`, its question code).
(b) `fresh_args`/`explicit_args` take the item: a const index gets `fresh(VarKind::Const)`; a
written const argument is checked against the declared type (E0308 on a mismatch). (c) blaze-lower
`type_args`: `GenericArg::Const` becomes a `PTy` in `type_lists`: a literal is parsed there
(`true`, a char, an integer; an unsuffixed integer is `ConstValue { ty: Infer, bits }`, whose type
`explicit_args` sets from the parameter's); a path or a block goes to a new `Types::const_arg(view,
ConstIdx, tys) -> PTy` hook (a parameter is `ConstParam(i)`, a block naming no parameter the
evaluator's value, its Need the item's); anything else is refused by name ("a const argument
expression over a generic parameter"). `repeat_len` becomes the same hook's form for an
expression.

**Full test run after step 3** (lanecheck, 2026-11-16):
- "test blaze-typeck: exit 0; 56 passed, 0 failed".
- "test blaze-build: exit 101; 73 passed, 1 failed". The failure is `tests::pointer_intrinsics_run`, which panics at tests.rs:2092: "callee ItemKey(5) at List { start: 0, len: 0 } is not placed before its caller". The test builds an array (`Expr::Array`, `aggregate_raw_ptr`), so whose failure this is remains UNDECIDED. tests.rs carries 1959 uncommitted lines of the code builder's own work. Check it first: run that test with step 3's blaze-build edits backed out (instance.rs, walk.rs, layout.rs, cabi.rs, tests.rs `array`).
- blaze-lower, blaze-consteval and blaze-ladder: no error of their own was reached. blaze-scope does not compile beneath them (resolver.rs `Stamps`, `Watch`, `Solver`; another lane's), so their tests are unverified.

**Next runner, first:**
1. Settle `pointer_intrinsics_run` as above.
2. Rerun the three unverified test suites once blaze-scope compiles.
3. Build step 4 exactly as designed above: the World's `const_params` question (tell the ladder lane the exact trait change first), const variables in `fresh_args`/`explicit_args`, and blaze-lower's `GenericArg::Const` with the `Types::const_arg` hook. Write the tests above with it.
4. Run the census (commands in /Volumes/Projects/blaze-int-1112-notes/census-1116/), before and after. The "a const argument" rows are 108 / 42 / 82.

**Queue after const generics, in order:**
1. Pattern types. The owner's hello world reads through them: `Duration`'s nanos and `NonZero`'s inner types in the pinned core. Write the spec paragraph here first. Then add the variant with all its arms. Then layout's valid range and niche, sent to the code builder's lane. Then the tests: `NonZero::<usize>::MIN`, `Duration::MAX`, `size_of::<Option<NonZero<u32>>>() == 4`, and an out-of-range value refused.
2. C-string literals: `&'static CStr` through its lang item, the bytes plus a NUL, and an interior NUL as a finding.
3. Destructuring assignment and `offset_of!` in blaze-lower's machine.rs.
4. The residue: E0282, then missing lang items, then E0369. Judge each by front-mode probes.

**Tests first** (blaze-lower typeck_tests.rs, each with its rule in its doc, a right and a wrong
program): `[0u8; 4].first_chunk::<4>()` is `Option<&[u8; 4]>` and `::<5>` against a
`[u8; 4]` expectation is E0308 at the call; a fn `fn n<const N: usize>() -> [u8; N]` called as
`n::<3>()` is `[u8; 3]`, and with `n::<true>()` is E0308 at the argument; a const param used in
a body (`fn f<const N: usize>(a: [u8; N]) -> usize { N }`); an inferred length
(`let a: [u8; _] = [1, 2];`).

**Residue after the forms lane** (bodies stopped by that cause alone, bstd / core+alloc /
tempo, forms census 2026-11-16), with the likely cause:

| alone | cause | likely cause |
|---|---|---|
| 286 / 208 / 200 | E0282 annotations needed | Mostly a world that knows no item: a call or method whose callee is unresolved leaves its result a variable; recount in front mode before fixing the checker. |
| 108 / 42 / 82 | a const argument | This section. |
| 89 / 21 / 19 | E0000 missing lang item | `format_args!` and its kin need the `fmt` lang items, which the forms census's empty world lacks; a world question, not a checker gap. |
| 40 / 28 / 12 | E0369 binary operator | Operands from unresolved paths (error-free variables) or operator traits the empty world lacks; recount with a world. |
| 28 / 2 / 0 | lowering: destructuring assignment | blaze-lower machine.rs refuses `(a, b) = e`; lower it as rustc does, to `{ let (x, y) = e; a = x; b = y; }`. |
| 26 / 5 / 1 | lowering: `offset_of!` | machine.rs refuses `builtin # offset_of`; needs an `Expr::OffsetOf` the checker types as `usize` after checking the fields exist. |

**Step 4, built (2026-11-16, next runner).** `World::const_params(item) -> ConstParams {
params: &[(index, PTy)], types }` (`Question::ConstParams`, an item record); implemented by toy,
fixture (`Fx::const_param`), blaze-build tests.rs, both ladder fronts (empty: their
`generic_names` refuse every const parameter) and blaze-real check.rs. `Machine::fresh_generic_args
(item, from, n)` gives a const parameter a const variable; `explicit_args(at, seg, item, from,
count)` checks each const argument against its declared type (`generic_arg`: an unsuffixed
literal takes the declared type; E0308 on a mismatch; a constant for a type parameter or a type
for a const one is the new `DiagCode::GenericArgKind`, E0747). A path to a const parameter as a
value (`Def::TyParam(i)` with `const_params` naming `i`) is its declared type. blaze-lower
`Conv::const_arg` reads a literal const argument (`true`, a plain `'c'`, an integer with radix,
`_` and suffix, a suffixed negative) into `ConstValue`; a block is refused by name ("a const
argument that is no plain literal ..."). Tests (blaze-lower typeck_tests.rs):
`an_explicit_const_argument_is_a_value_of_its_parameters_type`,
`a_const_argument_is_inferred_from_an_array`, `a_struct_generic_over_a_const`,
`a_const_parameter_is_a_value_in_its_body`, `const_literals_read_their_values`: "exit 0; 1
passed, 0 failed" each; the full run "test blaze-lower: exit 0; 78 passed, 0 failed". Census
(forms, files `port27/census-1116/forms-*-step4.txt`): "a const argument" 108 / 42 / 82 became
5 / 25 / 1, all blocks (`{ Answer::LEN }`, `{ U128_MAX_DEC_N - 16 }`).
Left: (1) the `Types::const_arg` hook for blocks, which needs a world that evaluates (the ladder
refuses const parameters today); (2) blaze-solve `fresh_args` (assembly.rs:256) still gives an
impl's const parameter a TYPE variable when it matches an impl (`impl<T, const N: usize> .. for
[T; N]`): pass the kinds through `ImplHeader` facts and use `VarKind::Const`; test with an impl
over `[T; N]`; (3) a literal out of its parameter's range (`n::<300>()` for `u8`) is not
refused; (4) fixture's `implied_sized` adds `Param(i): Sized` for a const parameter too (only
when a test declares `Sized`); (5) blaze-build has no arm for a value path `Def::TyParam(i)`.

**Next runner, first (the coordinator, 2026-11-16):** before pattern types' node, settle Left
(2): it can make a WRONG answer (a const position unified with a type variable), not only a
missing one. Either give an impl's const parameter a const variable (blaze-solve
assembly.rs:256 `fresh_args`, callers trait_goals.rs:238 and alias.rs:745 with `h.generics`:
the impl's facts must carry which indices are const) or refuse such an impl by a typed error
naming it. Test: an impl generic over `const N: usize` for `[u8; N]` is selected for `[u8; 4]`
with N = 4, and not where the lengths differ. Leave the `{ expr }` arguments as the named gap
(they wait for the evaluator's hook). Then pattern types, below.

## Pattern types (2026-11-16, spec)

**What the pinned core writes.** `pattern_type!(T is PAT)` (a builtin type macro, core/src/pat.rs)
in num/niche_types.rs (`define_valid_range_type!`: `Nanoseconds(u32 is 0..=999_999_999)`,
`NonZeroU8Inner(u8 is 1..)` and its kin for every unsigned width and `usize`,
`NonZeroI8Inner(i8 is ..0 | 1..)` for every signed width, `UsizeNoHighBit(usize is
0..=HALF_USIZE)`, `U32NotAllOnes(u32 is 0..u32::MAX)`, `I32NotAllOnes(i32 is ..-1 | 0..)`,
`NonZeroCharInner(char is '\u{1}'..='\u{10ffff}')`, `CodePointInner(u32 is 0..=0x10ffff)`) and
`*const T is !null` (ptr/non_null.rs `NonNull::pointer`; pat.rs `CoerceUnsized`,
`DispatchFromDyn`, `Unpin`, `Freeze` impls; marker.rs's `Copy` list). The patterns are: an
integer or `char` range (`a..=b`, `a..`, `a..b`, `..b`, ends literals or paths to consts), an
or of ranges, and `!null`.

**Meaning.** `T is PAT` is the values of `T` that match `PAT`, with `T`'s layout. Every pattern
core uses is a set of values of the base's bits, and each is a union of inclusive ranges; `!null`
is `1..=MAX` of the pointer's address. So the node is `PNode::Pat(base: PTy, ranges: List)` (and
`TyView::Pat`, blaze-solve `TyKind::Pat`, blaze-ty's term): `ranges` holds const terms in pairs,
`start ..= end` each, `start > end` meaning wrapping. The constructor canonicalizes: ranges
sorted, merged when they touch, and a run that ends at the base's maximum and one that starts at
its minimum merged into one wrapping range. So `i8 is ..0 | 1..` is `1 ..= -1` (wrapping), the
form layout needs.
- **Equality** is structural on the canonical form: same base, same ranges. This accepts a
  little more than rustc (`u8 is 1..` equals `u8 is 1..=255`), never less; coherence could
  only see two impls overlap where rustc does not, and core writes no such pair.
- **Relation to the base:** none implicit. Core goes `T -> T is PAT` and back only through
  `transmute` (`new_unchecked`, `as_inner`), and `transmute` needs equal sizes, which holds. No
  coercion, no field, no method of the base; a pattern type's impls are its own (pat.rs's).
- **Layout:** the base's size and alignment, with a valid range: the canonical range when there
  is one, so `Option<NonZero<u32>>` is 4 bytes (the niche is 0). blaze-build layout.rs already
  reads a valid-range attribute through `LayoutFacts`; the pattern node gives it the same answer
  (send the code builder the exact change: a `PNode::Pat` arm that returns the base's layout with
  `valid_range` from the node).
- **Constants in a pattern** (`HALF_USIZE`, `u32::MAX`) are evaluated by the front end that
  expands the macro, through blaze-consteval's `value_of`; the node holds values only (or a
  const parameter, for the extension).

**What blaze's extension adds** (extensions.md, edition.md change 8: `T is PAT` written
directly, over enum variants, with narrowing): the same node with `ranges` over the base's
VARIANT INDEX when the base is an enum (`Option<T> is Some(_)` is `1..=1`), an implicit and free
`T is PAT -> T` and `&(T is PAT) -> &T` coercion, and narrowing in `match`/`if let`. Patterns on
fields inside a variant (`Some(1..)`) need a nested form, not built. None of the surface is built.

**Order of saves.** blaze-solve `TyKind::Pat(Ty, List)` with all its arms (term, structural,
canon, fold, coherence, builtin: `Copy`/`Clone`/`Sized`/`Freeze` as its base, prefilter); blaze-ty;
then blaze-typeck `TyView::Pat`/`PNode::Pat` with portable.rs's hash tag and
`PortableTys::pattern(base, &[(u128, u128)])` as the canonicalizing constructor; then blaze-build
(layout, read_back), blaze-consteval, blaze-ladder, blaze-real examples, blaze-lower tests. The
`!null` pattern on a pointer: ranges `1 ..= usize::MAX` as `usize` values.

**Tests.** `NonZero::<usize>::MIN` and `Duration::MAX` check against core's own text (the front
end expands `pattern_type!`); `size_of::<Option<NonZero<u32>>>() == 4` (blaze-build); an
out-of-range value is not the type's (`transmute` keeps the bytes; a constant `0` written as a
`NonZeroU32Inner` through `new` is `None`).

**Built (2026-11-16, runner after step 4).** (1) Left (2) is settled: blaze-solve
`Cx::fresh_impl_args(&ImplHeader)` (assembly.rs) gives an impl's const parameter a const variable
(coherence's `fresh_for`, which reads `ConstParam(i)` from the impl's portable table). It is used at
trait_goals.rs and alias.rs `step_impl`. Tests (blaze-lower typeck_tests.rs,
`const_impl_world`): `an_impls_const_parameter_is_bound_by_selection`,
`an_impls_const_parameter_takes_one_value`. Without the fix: "exit 101; 0 passed, 2 failed".
A planted bug (two const values always equal in blaze-ty relate.rs) was caught: "9 passed, 4 failed".
(2) Pattern types, first saves: blaze-solve `TyKind::Pat(base, ranges)` and `Shape::Pat` with
arms in children, fold, canon (`T_PAT` 34), prefilter (34), coherence (non-local, like `[T]`),
Sized (always), Copy/Clone and auto traits (the base). blaze-ty `ty::PAT` 39 (Desc Child/Child,
child classes T and L), `TyKind::Pat`, `Tys::pat(base, ranges)`, print. blaze-typeck
terms.rs both ways, facts.rs shape, world `Shape::Pat`. Every crate checks ("check ok (all
targets)" for blaze-solve, blaze-ty, blaze-typeck, blaze-build; mixcheck for blaze-lower, blaze-consteval,
blaze-ladder, blaze-real). Tests: blaze-solve 5, blaze-typeck 56, blaze-ty 18 pass. blaze-lower:
81 passed and 2 failed, both scoped_tests `dollar_crate_*`, which are the expansion lane's.
**Next runner, first:** (a) the portable layer: `PNode::Pat { base, ranges }` and `TyView::Pat`
with all their arms (find them by `PNode::Slice` and `TyView::Slice`: blaze-typeck fixture,
machine, instance, portable.rs hash tag, rules/*; blaze-build walk, layout, instance, read_back
and tests; blaze-ladder front.rs, front2 types and buckets; blaze-consteval machine, code,
layout, fixture). infer/facts.rs:501 must map it to solve's `TyKind::Pat`, and
`PortableTys::pattern(base, &[(u128, u128)])` canonicalizes the ranges. (b) blaze-lower typeck.rs
lowers `pattern_type!(T is PAT)`. (c) layout's valid range: send it to the code builder. (d) the
tests in the spec above. (e) The ladder lane asked for `Abi::RustCall` ("rust-call") in
blaze-solve `term::Abi`, a fn pointer of that ABI relating only to itself, for core's `Fn*` heads.
Add it with all its arms and tell the ladder lane.

**Built (2026-11-16, next runner).** (a) `PNode::Pat(base, ranges)` and `TyView::Pat` with every
arm: blaze-typeck (portable.rs hash tag 24, `PortableTys::pattern(base, value_ty, width, ranges)`
over `canonical_ranges` with `MAX_PATTERN_RANGES` 16 and `Limit::PatternRanges`; instance,
machine, fixture, writeback, facts, infer both ways), blaze-build (instance tag 15, walk `repr`
as the base, layout.rs: the base's layout with the node's one range as the scalar's, or a wide
pointer's address's, valid range, so the niche follows), blaze-ladder (front.rs, front2/buckets.rs
`Shape::Pat`, run.rs ground). The checker keeps it opaque; a mismatch where one side is a
pattern type over the other is `DiagCode::PatternNotBase` (E0308, machine.rs `diag`). Core goes
between base and pattern type only by `transmute` (checked: niche_types.rs, non_null.rs), so no
cast or coercion was added. (b) blaze-lower typeck.rs: `parse_pattern_type(text, range)` and
`pattern_type(tys, base, &read, pointer_bits, range, eval)`, with typed `Refused::Form`s. Expansion
leaves `pattern_type!` an unexpanded `TypeRef::Macro` row (path + input text); the ladder's
`Types` (front2/types.rs `lf::TypeRef::Macro`) must call these two: NOT WIRED yet, so
`Duration::MAX` and `NonZero::<usize>::MIN` are not yet checked from core's text. Tests:
blaze-typeck `suite/pattern_types.rs` 4 pass (planted `InferPlant::PatIsBase` caught);
blaze-lower `typeck_tests::pattern_types` 2. Left: the ladder wiring; a layout test
(`Option<NonZero<u32>>` 4 bytes) in blaze-build; the census; then (e) `Abi::RustCall`.

**Built (2026-11-16, `extern "rust-call"` and `Fn*` calls).** `Abi::RustCall` (blaze-solve term.rs,
bits 5; fixture prints it); the ladder maps "rust-call" in heads::abi_of (its lane, done). A
rust-call fn pointer or fn item is not `Fn*`-callable (builtin.rs; `FnSigDef::RUST_CALL`, and
infer/facts.rs now fills `FnSigDef` flags from the fn's header: `unsafe fn` items stopped being
`Fn` too). rules/call.rs `overloaded_call`: a callee step that is no fn item, fn pointer, closure
or variable tries `Fn`, `FnMut`, `FnOnce` with a fresh `Args`, borrows the callee `&`/`&mut`/not,
records the method as the callee, checks the arguments against `Args`'s fields, types the call
`<T as FnOnce<Args>>::Output`; `Args` no tuple is `DiagCode::CallArgsNotTuple` (E0059). Tests:
suite/fn_traits.rs 3. Not built: `dyn Fn` (no `dyn` in the checker: item 1 above; the call path
needs nothing more once the solver has object candidates); the paren sugar `Fn(A) -> B` (the
ladder's front2/types.rs refuses it: asked); the check that a rust-call fn's last parameter is a
tuple (rustc: "functions with the "rust-call" ABI must take a single non-self tuple argument";
a signature check, owed where fn heads are read); arity mismatches report E0061, rustc E0057.

**Next runner (2026-11-16, after `Fn*` calls).** Pattern types' layout test is written by the code
builder (blaze-build layout/tests.rs `pattern_types_give_their_range_as_a_niche`, its lane: 75
pass), with two guards it added (range ends masked to the scalar's width; a base with its own
valid range refused by name). The core-text checks (`NonZero::<usize>::MIN`, `Duration::MAX`)
wait for the ladder to call `pattern_type` (its README, "Seventh port lane", item 4). Forms
census, tempo: census-1116/forms-tempo-step5.txt (819 files under 52 dirs, against step4's 800
under 48: tempo's tree grew, so it is not comparable row for row; 87.2% reach the checker, was
87.4%). Forms mode has no world, so the `Fn*` work cannot move it. Next, in order: C-string
literals (blaze-lower typeck.rs:564 refuses `Lit::CStr`; add `tc::Lit::CStr` with every arm:
lit_path.rs `lit_ty` as `&'static CStr` via a new `LangItem::CStr` (lang "CStr"; its arms in the
ladder's langs.rs), blaze-build walk.rs `lit` (code builder's lane: a wide pointer over the
bytes and their NUL) and blaze-consteval code.rs:961 (`Op::Str` wide over bytes+NUL); the
interior-NUL finding belongs where `lit::bytes` unescapes, not in the checker); then
destructuring assignment and `offset_of!` in blaze-lower machine.rs; then the residue.

## Trait objects (2026-11-16, spec)

Found: blaze-solve and blaze-ty already hold `dyn` (`TyKind::Dynamic`, `ExTrait`/`ExProj`/`ExAuto`
rows, the object candidates, `Unsize` to/between objects, `DYN_COMPATIBLE`); the checker's
`PNode`/`TyView` do not, and nothing sets the world's `NOT_DYN_COMPATIBLE`. The checker joins them.
- (a) Type. Reference, "Trait object types": "There may not be more than one non-auto trait, no
  more than one lifetime"; "Two trait object types alias each other if the base traits alias each
  other and if the sets of auto traits are the same and the lifetime bounds are the same".
  `PNode::Dynamic(list)` / `TyView::Dynamic(list)` over predicate nodes, as the solver's:
  `ExTrait(trait, args after Self)` first (absent for `dyn Send`), then `ExProj { trait_, name,
  args after Self, term }` sorted by (trait, name), then `ExAuto(trait)` sorted by `ItemKey`,
  repeats dropped. One constructor makes this form (`PortableTys::dynamic`), so equal types are
  one handle; order and repetition as written cannot reach a key (rule 25).
- (b) What it implements. "Trait object types": "the trait object implements the base trait,
  its auto traits, and any supertraits of the base trait"; "Dynamically sized types": trait
  objects are DSTs, so never `Sized`. The solver's object candidate gives the principal and its
  elaborated supertraits; an associated type of them is the bound term (`ExProj`); an auto trait
  holds only if listed (builtin.rs, `ExAuto`). `Sized`: the solver's sized rule lists `Dynamic`
  as unsized.
- (c) Method calls. "Method call expressions": the candidate types' "methods provided by a visible
  trait implemented by T"; for an object, the principal's and its supertraits' methods are found
  at its autoderef step whether or not the trait is in scope (as a param's bounds are). A method
  with `where Self: Sized` is "explicitly non-dispatchable" ("Dyn compatibility"): its `Self:
  Sized` obligation on the object fails, so the call is refused.
- (d) Coercions. "Type coercions", "Coercion types": "TyCtor(`T`) to TyCtor(`U`), where TyCtor(`T`)
  is one of `&T`, `&mut T`, `*const T`, `*mut T`, `Box<T>`, and where `U` can be obtained from
  `T` by unsized coercion"; "Unsized Coercions": "`T` to `dyn U`, when `T` implements `U + Sized`,
  and `U` is dyn compatible"; "`dyn T` to `dyn U`, when `U` is one of `T`'s supertraits" (an auto
  trait may be dropped, never added: the target's `ExAuto`s must be in the source's or hold of
  `T`). The pinned core declares `CoerceUnsized` impls for `&`, `&mut`, raw pointers (ops/
  unsize.rs, each `where T: Unsize<U>`) and alloc for `Box<T, A>` (boxed.rs:2514); `Unsize` has
  no impls: the solver's built-in `T: Unsize<dyn Tr>`. The checker's `coerce_unsized` asks the
  solver `Source: CoerceUnsized<Target>` (refs and pointers by `Unsize` of the pointees) when the
  target's pointee is a `dyn` or a slice, and registers it; never on a variable target.
- (e) Dyn compatibility ("Dyn compatibility"): "All supertraits must also be dyn compatible";
  "`Sized` must not be a supertrait"; "must not have any associated constants"; "must not have any
  associated types with generics"; each method dispatchable ("not have any type parameters",
  "does not use `Self` except in the type of the receiver", receiver `&Self`, `&mut Self`,
  `Box<Self>`, `Rc<Self>`, `Arc<Self>`, `Pin<P>`, "not be an async fn", no return-position
  `impl Trait`) or "explicitly non-dispatchable" (`where Self: Sized`). Each failure is
  `DynIncompatible { trait_, member, rule }` with the `dyn` type's range; computed in the checker
  from the trait's items (`TraitDef` gains `items`), not by the world's flag.
- (f) Lifetimes: the checker erases regions ("Not decided here": the borrow checker keeps its
  own), so `Dynamic` has no region and "Lifetime elision", "Default trait object lifetimes" is
  the borrow checker's; no type answer depends on a region, so no wrong answer follows here.
- (g) For the code builder: a method or `Fn*` call on an object resolves to the trait's method
  with `Self` = the `Dynamic` type in its recorded args; that self type is what makes the call
  dynamic. The vtable's content and order are the code builder's.

**Built (2026-11-16, trait objects, the type node only).** `PNode::{Dynamic, ExTrait, ExProj,
ExAuto}` (portable.rs; `PortableTys::dynamic` makes the one form, insertion-sorted by
`object_pred_key`, two principals `Malformed::ObjectPreds`, over 32 rows `Limit::ObjectPreds`;
`object_principal` exported), `TyView::{Dynamic, ExTrait, ExProj { args_term }, ExAuto}` (ty.rs),
every arm: instance.rs import/export/shape, machine.rs lower/shape/children
(`principal_args`), writeback.rs, infer/mod.rs mk/view (blaze-ty `dynamic`/`ex_*`, read back by
`as_pred`), infer/facts.rs (to the solver's `Dynamic`/`ExTrait`/`ExProj`/`ExAuto`), fixture.rs;
blaze-build (instance.rs 3, layout.rs, walk.rs: a `dyn` is refused by name, the code builder's to
lay out) and blaze-ladder (front.rs, front2/buckets.rs, run.rs). Checks: blaze-typeck test 63/0,
blaze-build and blaze-ladder check ok. NOT built, next in order: no test yet for (a) (write one:
`dyn A + Send + Send` and `dyn Send + A` are one handle; planted bug: drop the dedup); (b) the
solver answers once typeck hands it `Dynamic` (fixture: `dyn Tr: Sized` must fail, `dyn Tr:
Send` must fail unless listed); (c) method.rs `probe_method`: at a step whose view is `Dynamic`,
push the principal and its supertraits' methods as inherent candidates (split lit_path.rs
`param_bound_traits`'s supertrait worklist into a shared fn); (d) coerce.rs `coerce_unsized`:
the spec's solver form (pointee target `Dynamic`/`Slice`, `Unsize`/`CoerceUnsized` goals via
`may_hold`, then registered); (e) `TraitDef.items` in the world and the
`DynIncompatible { trait_, member, rule }` diagnostic (nothing sets `NOT_DYN_COMPATIBLE` today:
every trait is treated compatible); then the `&dyn Fn(u8) -> u16` call test, steps 3 to 5 of the
brief (messages, `c".."`, rust-call tuple check, E0057).

**Built (2026-11-16, trait objects (a) to (d)).** (a) `PortableTys::dynamic` also refuses two
bindings of one (trait, name) to two terms (E0719), as two principals; tests in
suite/dyn_objects.rs (`object_order_and_repeats_make_no_new_type`, control
`control_object_order` over `dynamic_unsorted`, `two_principals_or_two_bindings_are_refused`).
(b) the solver already answered; blaze-solve gains plants `ObjectSized`, `ObjectAnyAuto`,
`ObjectProjIgnoresBinding` (builtin.rs `planted`), case `auto_dyn_other_auto_no`, and three
controls in tests/mod.rs over the named cases. (c) method.rs `probe_method`: at a `Dynamic` step
the principal (`principal_args`) and its supertraits (lit_path.rs `elaborate_bound_traits`, split
out of `param_bound_traits`) are inherent candidates; a `where Self: Sized` method is refused by
its registered `dyn: Sized` clause (`Unsatisfied`); the callee row is the trait method with
`Self` = the object (`m<dyn Sub>`): what the code builder reads as dynamic. Plant
`MethodClausesDropped`. (d) coerce.rs `coerce_unsized`: a `dyn` pointee by `Unsize` (asked by
`may_hold`, then registered), one ADT's instances with a `dyn`/slice argument by
`CoerceUnsized` (`Box`), equal pointees and variables skipped; plant `UnsizeUnasked`. Tests
`methods_of_an_object`, `unsized_coercions_to_and_between_objects` (ref, mut, raw, Box, upcast,
drop auto, add auto refused) with controls. Checks: blaze-typeck test 70/0, blaze-solve 8/0.
NOT built, next in order: (e) dyn compatibility: `TraitDef` gains `items` (every World impl in
one batch: fixture.rs, blaze-ladder front.rs and front2, blaze-lower if it implements World),
`DynIncompatible { trait_, member, rule }` reported at the `dyn` type's range, computed in the
checker; its control is today's state (every trait compatible: `NOT_DYN_COMPATIBLE` is never
set); a generic method is (c)'s "not dispatchable" case and is tested there. Then the
`&dyn Fn(u8) -> u16` call test (call.rs: the `Fn*` probe at a `Dynamic` callee step), then
`c".."` (the plan in the note above), the rust-call tuple check, E0057. Queue only (not to
build now): arbitrary-width integers (`u24`, `i80`; names library `Builtin::Int { signed, bits
}`) have no checker type: `IntTy`/`UintTy` have the standard widths only; design in
docs/v2/plan/design/arbitrary-ints.md; the ladder's front2 refuses them as
`Why::NoCheckerType` and files 22 core impls nowhere (ladder's message, 2026-11-16).

**Built (2026-11-16, (e) the rule's home only, not wired).** crates/blaze-typeck/src/dyn_compat.rs
(`pub mod dyn_compat`): the facts a world states per member (`MemberFacts { name, shape }`,
`MemberShape::{Const, Type { generic }, Fn(FnFacts), Unread(Unread)}`, `FnFacts { receiver,
own_non_lifetime_params, where_self_sized, is_async, opaque_return, self_outside_receiver }`,
`Receiver::{Absent, Value, Ref, RefMut, Box, Rc, Arc, PinOfPointer, Other}`), `DynRule` (nine
rules, each `quote()`s the Reference), `member_verdict` and `own_verdict(sized_supertrait,
members)`. A by-value `self` counts as `where Self: Sized` (the Reference: "receiver type of `Self`
(i.e. `self`) implies this"; rustc accepts it). Tests `dyn_compat_each_rule` (13 cases) and
`control_dyn_compat_all_compatible` (plant `PLANT_ALL_COMPATIBLE` = today's state; 10 refusing
cases missed). Check: blaze-typeck test 72/0. NOT wired, the exact plan, one batch (rule d):
1. world.rs: `TraitDef` gains `members: TraitMembers<'w>` = `Known(&'w [MemberFacts])` |
   `NotKnown` (the world has not read them: a `dyn` of the trait is a named gap, never
   compatible); hash it in `impl ContentHash for TraitDef` (tags, `Name.0`, the enum tags).
   Re-export from lib.rs. `Limit::Supertraits` (error.rs) for the walk below.
2. dyn_compat.rs: `verdict<W: World>(reads, trait_)`: a fixed `[ItemKey; 64]` worklist (as
   lit_path.rs `elaborate_bound_traits`; over 64 is `Limit::Supertraits`) over `PClause::Trait`
   supertraits whose first arg is `Param(0)`; a supertrait whose `lang == Some(Sized)` is
   `SizedSupertrait`; `NotKnown` is the gap; returns `{ Compatible, Not { trait_, by, member,
   rule }, Gap { trait_, by } }`. Supertrait args naming `Self` (rustc refuses; the Reference
   does not list it): leave out, say so.
3. infer/facts.rs `fill_trait` (line ~626): `DYN_COMPATIBLE` only when `verdict` is `Compatible`
   (replaces the `NOT_DYN_COMPATIBLE` test; delete that flag from world.rs, its only reader).
4. Report: `DiagCode::DynIncompatible` ("E0038") with `because: Some(Because::Dyn { trait_, by,
   member: Option<Name>, rule })` (Because has no matcher outside typeck); a gap is a new
   `CheckError::DynUndecided { trait_ }` (update blaze-real check.rs's two exhaustive
   `CheckError` matches, lines ~549 and ~965). Where: `lower_body_ty(t)` gains `at: At` (11
   callers: machine.rs 606, control.rs 70, lit_path.rs 138/151/264, cast.rs 44, closure.rs
   82/131/135/162, block.rs 103); after lowering, walk the `PTy` subtree (machine.rs `children`,
   make it `pub(crate)`) for `PNode::Dynamic`, take `object_principal`, and report once per
   (body, trait) through a `dyn_seen: Vec<ItemKey>` on the machine; the body's own signature
   (control.rs 61/63) at `At::Body`. Once per trait: the verdict is computed per trait row in
   `fill_trait` and per body at the first use only.
5. Implementors: fixture.rs (`TraitRow.members: Vec<MemberFacts>`, derived in `trait_fn_own`
   from `self_param`/`inputs[0]`/own count/own `Self: Sized` clause, plus `trait_member(tr,
   MemberFacts)` for consts and async), blaze-build tests.rs 506 (`NotKnown`), ladder front.rs
   2877 (`NotKnown`), front2/world_items.rs 364 (`Known(&t.members)`: REAL facts, else tempo's
   `dyn` regresses: front2/dyn_ty.rs `member()` already reads every fact from syntax; turn its
   return into `MemberFacts`, `TraitHead.dyn_` into `members`, and its `dyn_compatible` walk
   calls `blaze_typeck::dyn_compat::own_verdict`; then front2's own refusal goes when (4)
   reports). toy.rs and blaze-real return no `TraitDef`: nothing to do.
6. The end-to-end test (suite/dyn_objects.rs): `trait Tr { const N: u8; }` then `let x: &dyn Tr
   = &0u8;` reports E0038 naming `Tr`, `N`, `AssocConst` once even when written twice; run it
   against today's state first and quote the failing line.

**Built (2026-11-16, (e) wired: items 1 to 6).** `TraitDef.members: TraitMembers::{Known(&[MemberFacts]),
NotKnown}` (hashed; `NOT_DYN_COMPATIBLE` deleted). `dyn_compat::verdict(reads, trait_)`: a fixed
`[ItemKey; 64]` worklist (`Limit::Supertraits`); `lang == Sized` on the principal (`dyn Sized`) or
a supertrait is `SizedSupertrait`; supertrait args naming `Self` are `GapWhy::SelfInSupertraitArgs`
(a gap, not leave-out). `fill_trait` sets `DYN_COMPATIBLE` only on `Verdict::Compatible`. Report:
`lower_body_ty(t, at)` and `lower_sig_ty` (the owner's signature, `At::Body`) go through
`lower_at`, which after lowering scans the lowered subtree (the memo) for `PNode::Dynamic` and
calls `Machine::check_dyn`: `DiagCode::DynIncompatible` (E0038, `name` = member, `Because::Dyn`)
once per trait per body (`dyn_seen`, rolled back with probes), a gap is `CheckError::DynUndecided
{ trait_, by, why }`. Implementors: fixture known (`fn_facts`, `trait_member`); blaze-build tests
and the old ladder `NotKnown`; front2 `member()` returns `MemberShape`, `TraitHead.members`, its
own walk and `Why::Dyn*` deleted (one home). Test before the batch: `left: []` vs `right:
[("E0038", ..)]` (failed); after: `dyn_incompatible_trait_is_e0038_once` and its control
`control_dyn_incompatible_unreported` pass; blaze-typeck test 74/0; build-ladder, real, lower check
ok. Gaps left: a `dyn` in a struct field or another item's signature is not reported as E0038
(only the solver's flag refuses its coercion, as E0277); not done: the runner's steps 3 to 5
(`&dyn Fn` call proof, C-string literals, `rust-call` head rule and E0057).

## `?` on the ladder: not the checker's inference (2026-11-16 06:40, runner after the dyn batch)

The ladder's "`from_residual` at `[Result<(), Error>, Error]`" was not a variable left
unresolved: his `main` did NOT check clean. A probe in blaze-ladder's tests/front2_main.rs
printed 11 diagnostics on the finished record (E0277 `Unsatisfied` and E0425 `UnresolvedPath`
at 52..66 and 52..93, the two `?`), and `front.index.lang("Break")` and `lang("Continue")`
both `None` (`branch`, `from_residual`, `Try` found): front2's lang index misses `#[lang]` on
enum VARIANTS (core's `ControlFlow::{Continue, Break}`), so `World::lang_path` answers `None`,
the arms' patterns are E0425, the residual binding is the error type, and `from_residual`'s `R`
is the error type with it. blaze-ladder's `check_body` returns `Outcome::Done` with diagnostics
as checked (front2/check.rs, `Outcome::Done(done) if !round_gaps => return Ok(done)`), so the
walk read it as clean. Both are the ladder's (passed on). The fixture form of the same body
already checks to the right callees (`question_mark_through_try_and_from_residual`).

Built (the class): the writeback refuses a body with no diagnostic that records the error type
or a hole in an expression's, pattern's or binding's type, an adjustment or a callee's
arguments: `CheckError::UnknownInCleanBody { at: Recorded }` (error.rs; one linear pass over the
portable table, children before parents, no recursion: `first_unknown` in writeback.rs). Every
suite body goes through it. Test: `a_clean_body_never_records_an_unknown_type` (a world whose
signature answers the error type); before the scan it failed with "a clean record with an
unknown type passed: 0 diagnostics"; after: blaze-typeck test 75 passed, 0 failed. Owed next:
a lang path the world cannot answer is reported as E0425 with no name (`path_res`): it should be
`MissingLangItem` naming the lang item; the runner's steps 2 to 5 above are still open.

## Lang items the world cannot answer, `?` fixtures, E0057 (2026-11-16, runner after the self-check)

Built: one finding for every lang item asked and not answered: `DiagCode::MissingLangItem` with
`because: Because::NoLangItem(LangItem)` (`Machine::need_lang` / `missing_lang`). Sites: a
desugaring's lang path (`path_res` now takes `at` and returns `Result<PathRes, Unresolved>`;
`Unresolved::{ByName, Reported}`, E0425 only for `ByName`), operators (binary: `f.b == 2`
reported at the end unless builtin; unary `!`/`-`; `*` with no `Deref`), `Index`, `Fn*` in
`overloaded_call` (the first missing of `Fn`, `FnMut`, `FnOnce`, call checked no further),
`Unsize` and `CoerceUnsized` (finding, coercion taken: no second error as E0308), `.await` and
`format_args!`. Not reported (probes, not forms): autoderef's `Deref`, closure hints, the
`Sized` comparison. E0277 with no `where const` reason now names the bound:
`Because::Bound(trait)`, `found` its self type, `expected` its first argument after `Self`.
Tests (suite/lower_forms.rs on rung 7c, extended with `Option`, `impl<T> From<T> for T`,
`struct Other`, `fn same`, `fn opt`, `rung_7c_without(drop)`): `question_mark_without_continue_
names_the_lang_item` (with the fix removed: "left: [(UnresolvedPath, None)]"), `..on_result_with_
the_same_error_type` and `..on_option` (all types, and `select_impl` at the recorded
`from_residual` arguments picks the right impl), `..with_no_from_impl_is_e0277`, and the
existing two-`?` line and no-`FromResidual` case (now naming `FromResidual`, `u32` and the
residual). `wrong_count_through_fn_is_e0057` (suite/fn_traits.rs): `Fn*` calls check the count
against `Args` in `overloaded_call` as `DiagCode::FnTraitArgCount` (E0057). blaze-typeck test:
80 passed, 0 failed; ladder, lower, real, build check ok.

Owed, in order: (1) the no-`From` E0277 names the root bound (`Result<u8, Error>:
FromResidual<Result<Infallible, Other>>`), not rustc's leaf `Error: From<Other>`: the solver's
error report should carry the failing nested where-clause. (2) A closure callee with the wrong
count is still E0061 (the `TyView::Closure` arm in `expr_call`); rustc says E0057. (3) The
E0057 test was not run against the old code (it would have been E0061 from `start_args`).
(4) Not started: the `&dyn Fn(u8) -> u16 = &g; f(1)` proof (needs a world with `Fn*`, `Tuple`,
`FnOnce::Output` bound on the object, and `Unsize`: suite/dyn_objects.rs `objects_world` plus
suite/fn_traits.rs `world`), E0038 for `dyn` in a struct field or another item's signature,
C-string literals (queue item 2 above), the `rust-call` last-parameter rule.

## Every finding has a place; paths from primitive types (2026-11-16, runner after the E0057 batch)

Built: (1) `lower`/`lower_at`/`lower_node` take the `Cause` of the place being lowered, so a
projection (`PNode::Alias`) in a callee's signature, a method's, an impl header's, a field's or a
path's is registered at that call, path, field or pattern (`fn_def_sig`, `impl_args`,
`struct_field_ty`, `lower_list` take a cause; `At::cause()`; probes pass `Cause::Body` and roll
back). Test `projection_in_a_callee_signature_fails_at_the_call` (suite/mod.rs, `made<T>(t) ->
<T as Make>::Out` at `B`); with the old cause it printed "left: [(Unsatisfied, 0, false)]".
The invariant: writeback refuses a diagnostic with an empty range unless
`DiagCode::covers_the_item` (MissingLangItem, DynIncompatible) as `CheckError::DiagWithoutPlace
{ code, name }`; test `a_diagnostic_with_no_place_is_an_internal_error` (without the check: "an
E0277 with an empty range passed: Ok(1)"). E0317 now points at the `if`, the body's return
mismatch at the body's root expression. (2) `Def::Prim(Prim)` (world.rs; `Prim` = Bool, Char,
Str, Int, Uint, Float): a type-relative path from a built-in type goes through `assoc_value` on
the primitive's `TyView` (inherent first, then traits in scope). The fixture has `impl_const`,
`Bb::qpath`; tests `path_from_a_primitive_type_names_its_inherent_const`,
`..calls_its_inherent_method`, `qualified_path_on_a_primitive_type_names_its_const` (a
control: it passed before), `unknown_item_on_a_primitive_type_names_both` (before: three failed,
e.g. "got [(UnresolvedPath, 0)]"). blaze-typeck test 86 passed, 0 failed; lower, build check
ok; real ok through mixcheck (the ladder is broken by its own lane's `Env::anon` edit). The
ladder lane was told what `resolve_path` must answer.

Owed, in order: (3) the whole failed goal in an E0277 (trait or projection, every argument, the
term) and the leaf where-clause (`Error: From<Other>`): a batch over `Because`/`Diag` and every
user; (4) the `&dyn Fn(u8) -> u16` call proof; (5) E0038 for a `dyn` in a field or another item's
signature, C-string literals (blaze-consteval code.rs arm), the `rust-call` tuple rule, a
closure's wrong count (E0057, `TyView::Closure` in `expr_call`). (6) From the ladder
(abb2583784718f509): front2 makes each `impl Trait` parameter an anonymous own type parameter
after the declared ones (`FnHead.own` counts them; bstd out.rs:122 `Out::line`, `what: impl
Show`); explicit generic args give only the declared ones, so `explicit_args` must accept fewer
than `own` there and make the rest fresh (the Reference, "Impl trait": "explicit generic
arguments cannot be given for an anonymous type parameter"); front2 offers a declared-count
field: take it into the fn signature's answer rather than guessing.

## Trait arguments by type; the whole failed goal (2026-11-16, runner after the place batch)

Built: (1) `world::ArgsAfterSelf` (exported with `SplitSelf`): `ImplHeader::trait_args` is it,
built only by `ArgsAfterSelf::split(types, with_self, self_ty)` (takes a `Self`-first list apart
against the self type; the same table's slice, no re-intern) or `NONE`; both ladder fronts
(front.rs, front2/heads.rs) split through it. `Reads::impl_header` checks the count against the
trait's `TraitDef::generics - 1`: `CheckError::ImplTraitArity { impl_, trait_, expected, got }`.
Test `an_impl_header_with_self_first_is_refused_by_name` (lower_forms.rs); with the check off:
"a self-first impl header passed with 1 diagnostics". NOT done of (1): `PClause` trait clauses,
`PNode::ExTrait` and the solver's `Goal`s are still bare `List`s (clauses and goals `Self`-first,
`ExTrait` after `Self`): next, an `ArgsWithSelf` twin for clauses and goals and `ArgsAfterSelf` in
`ExTrait`, each a batch over the ladder's front2 and blaze-build.
(2) `Because::Bound(ItemKey)` is gone: `Because::Unmet { goal: UnmetGoal, leaf:
Option<UnmetGoal> }`; `UnmetGoal::{Trait, Projection}` holds every argument (`Self` first, a list
of the record's table) and a projection's term; `PendingDiag::unmet` carries `PendingGoal`s to
writeback (`export_goal`). The leaf: `Machine::failed_leaf`, a loop bounded by `MAX_LEAF_DEPTH`
(16): while exactly one impl's header matches, its first trait where-clause that `may_hold`
refuses becomes the goal; probes rolled back, the leaf's arguments resolved first. blaze's
choice: the leaf is what a person can fix, so front2's `diag_about` prints it first. Test
`question_mark_with_no_from_impl_is_e0277` wants "Result<u8, Error>: FromResidual<Result<
Infallible, Other>> because Error: From<Other>"; with the leaf search off it printed the goal
alone. blaze-typeck 87 passed, 0 failed; build, lower check ok; ladder, real ok via mixcheck
(blaze-scope broken underneath by its own lane). Owed, in order: a projection-goal E0277 test;
then the queue above unchanged from (4): the `&dyn Fn` call proof, E0038 in a field or another
item's signature, C-string literals, the `rust-call` tuple rule and the closure's wrong count, (6).

## The `&dyn Fn` proof; anonymous generics at a call; E0038 per item (2026-11-16, runner after the goal batch)

Built: (1) `a_dyn_fn_is_called_through_its_object` (suite/fn_traits.rs; the `Fn*` world plus
`Unsize` and `fn h() -> u16`): `let f: &dyn Fn(u8) -> u16 = &k; f(1)` is `u16`, `&k` unsizes,
no diagnostic. The checker needed nothing; the FIXTURE was wrong: `fn_facts` counted
`<Self as FnOnce<Args>>::Output` in `call_mut`/`call` as `Self`, so `dyn Fn` was E0038 (before:
"diagnostics: want [], got [(Mismatch, 1), (NotCallable, 2), (DynIncompatible, 1000)]"). Now an
alias whose self type is `Self` passes, as front2's dyn_ty.rs already did. The fixture printer
shows an object's `ExProj` as `Trait<args>::name = term` (its args follow the erased `Self`).
(2) `FnSig::declared_count` (every world: fixture `Fx::anon_params`, toy, blaze-build tests,
front.rs = own, front2 `FnHead::declared` from heads.rs's `declared`, ctors = generics):
`Machine::explicit_fn_args` (lit_path.rs; the three fn call sites, method.rs too) takes the
declared params from a turbofish and makes the anonymous ones fresh; a wrong count is E0107 with
`Because::ArgCount { item, expected, got }` (every E0107 now carries it). `Bb::path_args` writes a
turbofish. Tests suite/anon_params.rs; with the old count: "diagnostics: want [], got
[(GenericArgCount, 0)]" and "left: []" for the too-many case. (3) A projection goal that cannot
hold: `a_projection_that_cannot_hold_names_the_projection_and_its_term` (lower_forms.rs): E0277 at
the path with `UnmetGoal::Projection` `<B as Tr>::Out == u8`; passed on the code as it was.
(4) `dyn_compat::item_dyn_findings(world, item, out)`: E0038 for a `dyn` in an ADT's fields or a
fn's signature, once per trait per item, empty range (covers the item); test
`dyn_in_a_field_or_a_bodiless_signature_is_e0038`. NOT wired: a front end must call it for each
struct/enum/union and each bodiless fn (front2's walk: the ladder lane was told). blaze-typeck 92
passed, 0 failed; blaze-build test 94/0; lower ok; real ok (mixcheck, no snapshot taken).
Owed, in order: from the ladder (runner fifteen, front2 walk of his main): (a) `[0; HELD]` into a
field `[u8; HELD]` (`HELD: usize = 1024`, front2 gives `ConstValue{usize, 1024}` and the repeat
length as u64) leaves an expression's type unknown: `UnknownInCleanBody { at: ExprTy(ExprId(3)) }`
in `Held::new` (blaze-std out.rs), three of this class; (b) `Solve(Malformed(Pred))` in a derived
`Clone::clone` of `enum Source { Process(&'static [&'static str]), Given(Arc<[String]>) }`
(blaze-std args.rs:13, env.rs:160): give `Malformed::Pred` the predicate or clause index so the
lane can tell whether front2 built it; then C-string literals, the `rust-call` tuple rule, the
closure's wrong count (E0057), the `ArgsWithSelf` twin.

## Repeat lengths are expressions; `PointeeSized` holds; `Malformed::Pred` names its goal (2026-11-16)

(a) DONE. `Expr::Repeat { elem, len, count: Option<ExprId> }`: `count` is the length's own expression
(`HELD`), typed as `usize` (state 1 of `expr_repeat`). blaze-lower's typeck.rs passes `Some(len)`.
Before this, the length expression was never visited, so its type was left unknown
(`UnknownInCleanBody { at: ExprTy(ExprId(2)) }` in the test). Test
`a_repeat_into_a_declared_array_of_a_constant_length` (const_terms.rs) covers a const path, a
literal and `_`. New fixture helpers: `Fx::konst`, `Bb::repeat_counted`. Not done: an expression
that is never visited is still caught only by the writeback net, not by a diagnostic.
(b) DONE. `Malformed::Pred { at: &'static Location, goal: Option<BadPred> }`: `at` is the solver
line that refused it (`error::bad_pred`, `#[track_caller]`). `goal` is filled by
`eval::drive` from the top frame (`bad_pred_of`). The real cause was core's `impl<T: PointeeSized>
Clone for &T`: `[u8]: PointeeSized` was refused at builtin.rs:342. Now that goal holds (RFC 3729).
Test `a_pointee_sized_bound_on_an_unsized_argument_holds` (lower_forms.rs). Before the fix it
failed with `Pred { at: builtin.rs:342, goal: Some(BadPred { kind: Trait, args: [Some(Ty(Slice))..] }) }`.
State: typeck 94/0, solve 8/0, blaze-build 94/0 (mixcheck), blaze-real ok (mixcheck). The ladder
lane was told. Owed: C-string literals, the `rust-call` tuple rule, E0057 for a closure, the
`ArgsWithSelf` twin.

## C-string literals; what forms mode's E0282 is (2026-11-16, runner after the repeat batch)

(1) DONE: C-string literals. `tc::Lit::CStr` (body.rs) typed `&'static CStr` through the new
`LangItem::CStr` (lang "CStr"; ladder front2/langs.rs arm), missing lang item reported at the
literal (`lit_ty` now takes the `At`). blaze-lower typeck.rs passes it on. blaze-consteval:
`lit::c_bytes` unescapes (`\xNN` any byte, `\u{..}` UTF-8, raw keeps `\`), appends the NUL, and
refuses an interior NUL as `LitError::InteriorNul { at }` (offset in the token; the
`ConstError.at` carries the literal's range); `LitError::CStr` is gone. code.rs `Lit::CStr`:
a wide pointer over bytes+NUL. `layout::is_unsized` now follows a struct's last field (bounded,
64) so `&CStr` is wide; its three callers take the world. blaze-build walk.rs refuses
`Lit::CStr` by name (owed to the code builder: the same wide pointer). Tests:
`a_c_string_literal_is_a_static_cstr` (blaze-lower; failed against the old arm: "exit 101; 0
passed, 1 failed") and `c_string_literals_end_in_one_nul` (blaze-consteval). Suites: lower 93/0,
consteval 25/0, typeck 94/0, build 94/0; blaze-real ok (mixcheck); ladder's own lane had
front2/program.rs broken at 07:39 (not this batch). bstd census: the C-string row (21) is gone
(census-1116/forms-bstd-step6.txt).
(2) CLASSIFIED, NOT FIXED: bstd's E0282 (278 alone / 436, step6). The 28 samples the census
prints are ALL world artifacts: ~20 a closure parameter whose callee is a method the empty world
cannot know (`partition_point(|it| ..)`, `self.with(|cell| ..)`, `find_map(|line| ..)`), ~7 a
cast whose source is a method result or param the world cannot type (`handle.len() as u32`,
`self as i32`), 1 a test closure. Forms mode's `Empty` world answers no `fn_sig`, so even a
body's own params are unknown. No checker class was found; judge E0282 in front mode instead.
New in step6: "check: unknown in a clean body ExprTy" 1826 bodies (clean fell 22.6% -> 8.1%):
the writeback net counts the error type as unknown, and with no own signature params are the
error type silently: an artifact of `Empty`, not a regression (probe: `fn d(x: u32) -> bool
{ x == 2 }` is clean in a fixture world with its sig). The census lane should give forms mode the
written signature or class these apart.
(3) Destructuring assignment and `offset_of!` (runner after the C-string batch; see the next
section for what was proved by a run).

## Destructuring assignment; `offset_of!`; tuple-struct field names (2026-11-16)

(1) Destructuring assignment: machine.rs `binary` sends an assignee that is a tuple, array, call
or struct (parens looked through; `..` alone and `_` alone stay `Expr::Assign`) to desugar.rs
`destructuring_assign`: `{ let PAT = rhs; place_1 = $made; .. }`, the left side walked breadth
first (a node's children consecutive), patterns made from the last node back (post-order kept),
assignments in written order by a preorder stack. `..` is a rest only under a tuple, slice or
tuple struct; `S { a, ..base } = e` is `DiagKind::FruInDestructuringAssignment` at the base.
`Why::DestructuringAssignment` is deleted; BODY_KEY_DOMAIN is 5. `(a, b) += ..` is now E0067
(`DiagCode::InvalidCompoundLhs`, op.rs `expr_assign_op`), not E0070. Tests: blaze-lower
`destructuring_assignment_is_a_let_then_assignments` (dump: rhs calls before the places'
index calls) and `destructuring_assignment_types_each_place`.
(2) `offset_of!`: machine.rs lowers OFFSET_OF_EXPR to `Expr::OffsetOf { ty, fields }` (names in
`ids`; a `0.1` float split at its dot); new `tc::Expr::OffsetOf { ty: PTy, fields }` (fields in
`segments`), field.rs `expr_offset_of`: `usize`, each step a field of the type before (tuple,
struct/union, enum variant then its field), no autoderef; a missing step or a path ending on a
variant is E0609 at the form. Privacy is NOT checked (the world has no field visibility). Value:
blaze-build walk.rs and blaze-consteval code.rs refuse "`offset_of!` (a field's offset from the
layout)": owed: `layout` must give field offsets. Test `offset_of_is_a_usize_over_a_checked_field_path`.
(3) Found on the way: field.rs looked a tuple struct's field up as `Name(index)` but the world
names it `Name::of("0")` (front2 heads.rs, the fixture): every real `p.0` on a tuple struct was
E0609. New `Name::of_index`; same test.
(4) For main (not built). `Lit::CStr` in blaze-build walk.rs needs: the bytes with their NUL
(consteval `lit::c_bytes`) as a read-only data object of the function, and the value a wide
pointer `(ptr, len)` where `len` counts the NUL (`&CStr` is `&[c_char]`-shaped, as code.rs
builds it). Arbitrary-width ints (`u24`, `i80`; names give `Builtin::Int { signed, bits }`
under `#![blaze::arbitrary_ints]`): `IntTy`/`UintTy` need a `Bits(u16)` case (or one
`Int { signed, bits }` node) in blaze-ty's interned kinds and `PNode`; literals: an
`IntSuffix` is never written for them, so a literal takes the type only by inference and its
range check is `bits`-wide; casts: int-to-int at any width (`as` truncates or extends by
`bits`); operators: the builtin arithmetic, bit and shift rules accept any width (the `Add`
etc. impls come from the library for the standard widths only, so the checker's builtin case
must cover them); layout: size `ceil(bits/8)` rounded up to a power of two (align likewise,
capped at 16), as the design doc says; consteval and LIR carry the width per operation.
(5) Runs so far (lanecheck blaze-lower): the dump test passed; `typeck_tests::` "29 passed, 1
failed" (offset_of and `p.0` passing; the typed destructuring test refused by to_typeck:
"struct field defaults (`..`)" — the orphaned assignee nodes were converted; fixed by writing
them `Expr::Missing` after the desugar). NOT YET RUN after that fix: the full lower suite (a run
was queued behind another lane's ladder mixcheck), typeck, consteval, build, blaze-real, the
ladder. No run against the old code was quoted for the new tests (the old code refused both
forms by name: "unhandled BIN_EXPR (DestructuringAssignment)", "unhandled OFFSET_OF_EXPR").
(6) Next runner (2026-11-16): the assignee is not a value (Reference, "Assignee expressions"):
after the desugar no root reaches its own nodes, but two passes read EVERY node in order
(to_typeck's conversion; blaze-build walk.rs `object_reads` and `scan_flags`), so every
non-place node, a tuple struct's callee path and every parenthesis around a node is written
`Expr::Missing`; the checker types only from the root (machine.rs `expr_ty` starts `err`), so it
never sees them; a place is read only by its `Assign`. Positional names: `Name::of_index` now
also in blaze-ladder front.rs and front2 heads.rs, both fixtures (typeck, consteval); test
`ids.rs a_positional_name_is_its_decimal_text`; no other `Name(index)` site (toy.rs's
`Name(i)` is a toy variant's name, not a field). Closure call arity: E0057
(`FnTraitArgCount`), was E0061 (call.rs closure arm). `extern "rust-call"`: control.rs registers
`last param: Tuple` (E0277 at the param). OWED: a rust-call fn with NO parameter (no place in
the body; the fn head's reader must report it); the `Self`-first twin of `ArgsAfterSelf`.
Tests: lower typeck_tests `a_closure_called_with_the_wrong_count_is_e0057`,
`a_rust_call_fn_takes_its_arguments_as_one_tuple`, `S { g, .. } = s` added to the typed one.
Runs (lanecheck, after the edits): blaze-typeck "95 passed, 0 failed"; blaze-consteval "25
passed, 0 failed"; blaze-build "94 passed, 0 failed"; blaze-real and blaze-ladder "check ok (all
targets)"; blaze-lower "96 passed, 2 failed": `destructuring_assignment_types_each_place` now
panics at typeck_tests.rs:74 (to_typeck passes; `check` returns a CheckError, its text NOT seen:
lanecheck's filter drops the panic message; next: find the test binary lanecheck runs, it is not
under targets/lane/debug/deps by name, and read the message), and the closure E0057 test at :67
(to_typeck refused, likely `|x: u32|`: rewritten to `|x|`, NOT rerun). `--test ladder_front2`:
started, its result line not captured. The rust-call test: passed (not in the failed list).

(7) Runner after (6) (2026-11-16, measured by census-C of bstd):
- Destructuring assignment: the `CheckError` was the checker's clean-body rule, not the desugar:
  "UnknownInCleanBody { at: ExprTy(ExprId(2)) }": the orphaned assignee nodes the lowering writes
  `Expr::Missing` are never reached, so they keep `err`. writeback.rs `first_unknown` now skips
  `Expr::Missing` rows (`Recs::holes`: a hole carries no value). typeck_tests "32 passed, 0
  failed" (the E0057 closure test passed too). `|x: u32|` in the lower tests is refused by the
  TEST world ("a written type (the test world resolves none)", with an EMPTY range 0..0: a bug
  of that refusal), not by `to_typeck`'s closure rule.
- `DiscriminantKind::Discriminant`: blaze-solve alias.rs `Rule::Discriminant`, builtin.rs
  `discriminant_ty` (enum: `AdtDef::discr`; coroutine `u32`; else `u8`; param/alias rigid).
  The repr comes from `World::attrs`: `TypeAttrs::with_discr(DiscrRepr)` (bits 8..12);
  `DiscrRepr::Unread` stops with `Unsupported::ReprNotRead(def)`. Front2 answers 0 (Unread)
  until the ITEMS heads lane answers (asked once, a47dcd9b01f5eb34d). Fixtures
  suite/discriminant.rs "3 passed"; without the rule "0 passed, 3 failed" (`[Unsatisfied]`).
  OWED: `Pointee::Metadata` is NOT normalized (no rule; rustc's `consider_builtin_pointee_candidate`).
- Closure to fn pointer: coerce.rs arm, `AdjustKind::ClosureFnPointer` (new; blaze-build and
  consteval refuse it by name: OWED the closure body as a fn), captures by closure.rs
  `captures_nothing` (the syntactic capture walk, rows dropped). Fixture fn_ptr.rs
  `a_closure_that_captures_nothing_is_a_fn_pointer`; without the arm "left: [Mismatch] right: []".
- NOT DONE: (4) UnknownInCleanBody PatTy (guest_grant PatId(4) is, counting params first and
  post-order, `Reach::Shared { .. }` under `match &self.reach`; stopped's PatId(7) is
  `FsKind::OverQuota(..)`: both a struct/tuple-struct pattern with only a rest under match
  ergonomics: reduce to a Bb fixture first); (5) DiagWithoutPlace Mismatch; (6) naming
  projection goals in `Because`. blaze-ladder did not compile at hand-off (preds.rs `Sizing`,
  heads.rs `const_params`: other lanes' edits in progress).

(8) Runner after (7) (2026-11-16):
- PatTy UnknownInCleanBody (22 bstd bodies): the `..` of a tuple, tuple-struct or struct
  pattern is a node in no list (its parent's `rest`), never reached, so it kept `{error}`
  (the reference scrutinee was not the cause). to_typeck maps it to `Pat::Missing` (a slice's
  `..`, and the `..` under `x @ ..`, stay `Wild`: `Conv::slice_rest`); writeback `pat_holes`
  skips `Pat::Missing` rows as `holes` does for expressions. Fixture typeck_tests
  `a_rest_pattern_under_a_reference_is_a_hole`: before "check refused UnknownInCleanBody { at:
  PatTy(PatId(4)) }", after "1 passed, 0 failed".
- The solver's param-env refusal (eval.rs `env`, "Malformed(Pred { .. line: 312 .., goal:
  None })", hello world's core `From::from` of error_union.rs): a where-clause's projection
  (`Inject<S, <U as UnionFrom<S>>::Path>`) was lowered as in a body, to a fresh variable plus
  an obligation; the env refuses variables. New `Lower::Assumed` (control.rs `start_body`):
  the alias stays `TyView::Alias`, a hole is `HoleInAnswer`. The env's refusal now names its
  clause (`bad_pred_of`). UNPROVEN: no fixture yet. The lower tests' world could not reach the
  clause (`t.go()` on `&T`: E0599 NoMethod; `Inject::go(t)`: UnresolvedPath): write it in
  blaze-typeck's suite (fixture.rs builders, as suite/mod.rs `bound_proved_through_where_clause`)
  with a `T: Inject<<T as Other>::A>` clause, and quote it failing with control.rs back on
  `Lower::Body`; if it cannot be shown, revert the `Assumed` mode.
- DiagWithoutPlace Mismatch (9 bstd, 4 alloc: `capacity_overflow`, `panic_arc_overflow` are
  `-> ! { panic!("lit"); }`; the rest `None => panic!(..)` arms): every one is a `panic!`
  (core's `panic_2021` is `({ panic_fmt(const_format_args!(..)); })`) where a non-`()` type is
  expected. Written by hand the same shapes check clean (control
  `a_macro_block_that_diverges_is_never`), so the cause is in front2's lowering of that
  expansion (most likely the block's divergence lost, or its range empty: the empty range is
  why it has no place). NOT FIXED: needs a run of front2 on one of these bodies.
- NOT DONE: (3) naming projection goals in `Because`; (4) `Pointee::Metadata`; (5) E0283 in
  kv.rs; (6) the test world's empty-range refusal (typeck_tests.rs `NoTypes`, and to_typeck's
  "a parameter with no pattern" has `Default::default()` too); (A, from main) const arguments
  that are no plain literal (`{ AtomicOrdering::SeqCst }`, 28 in the hello world): needs a new
  `Types::const_arg` (front2 evaluates; ADT const values' representation in `ConstValue` is
  not decided: an enum value's bits) and every `Types` implementor (12, incl. blaze-real,
  blaze-proc-macro's example): a batch.
- For main (code builder refusals): `Lit::CStr` needs the `&CStr` constant's bytes with the
  NUL (a static); `AdjustKind::ClosureFnPointer` needs the closure's body emitted as a fn with
  the pointer's signature (no captures: `captures_nothing`); `offset_of!` needs field offsets
  from layout (the checker types it `usize` only).

(9) Runner after (8) (2026-11-16):
- `Lower::Assumed` PROVEN: suite/where_projection.rs (`T: Inject<<T as Other>::A>`, `t.go()`);
  with control.rs `lower_body_list` on `Lower::Body`: "Solve(Malformed(Pred { .. BadPred {
  kind: Trait, .. args: [Some(Ty(Blanket)), Some(Var), None, None] .. }))"; with it, passes.
  blaze-lower's two reds were already green ("102 passed, 0 failed").
- Const arguments that are no literal: new `Types::const_arg(view, c, tys)` (blaze-lower
  typeck.rs; `no_const_arg` is the typed refusal at the argument's own range for callers with no
  constants); every implementor changed (blaze-lower tests, typeck_tests, expand_tests,
  blaze-consteval fixture, blaze-real's three examples, blaze-proc-macro's tempo_pipeline,
  blaze-ladder front.rs and front2 body.rs). front2's `BodyTypes::const_arg` reads `{ e }`
  through `Builder::const_term` (the array length's home: const parameter, literal, const
  item; the type left `Infer` for the checker to take the parameter's) and a path to an enum
  variant as `PortableTys::variant_value(enum, index)`. DECIDED: an enum value is
  `PNode::ConstValue { ty: Adt(enum, []), bits: variant index }`, made only by
  `variant_value`; the solver holds it as `ValTy::Enum(Def)` (canon: code 14, the def's four
  words, then the bits). Fixtures suite/const_enum.rs (the parameter's enum checks; another
  enum is E0308). OWED: a generic enum's variant (`{ None }` for `Option<u8>`); `{ N + 1 }`
  still refuses as `Why::ConstPath`; not yet run on the hello world's 28 `atomic_*` bodies.
- NOT DONE (coordinator's order): (3) UnknownInCleanBody ExprTy in `core::panic_bounds_check`
  (every program reaches it: port27/front2/entry-1.txt) and the 36 bstd `fmt` impls, with the
  13 DiagWithoutPlace Mismatch of `panic!` arms (expansion lane: machine.rs
  `coerce_many_push(site, None, ..)` reports `At::Body` when the tail-less block's statement did
  not register as diverging; the statement is `panic_fmt(const_format_args!(..))`).
  `expr_format_args` (rules/async_fmt.rs) does type its own node, so look at what front2 makes
  of the captured `{len}` argument, or of the `!` call's divergence; and give that mismatch the
  block's own range. Then (4) naming projection goals in `Because`; (5) `Pointee::Metadata`
  through the closures lane's `LangItem::assoc_type`, no second way; (6) E0283 in kv.rs.
  Fixture worlds must answer as front2/world_items.rs does.
- Asked by the heads lane (types and bounds, 2026-11-16): `ImplHeader` has no polarity, so
  core's 27 and alloc's 12 negative impls (`impl<T: PointeeSized> !Send for *const T`,
  `!Clone for &mut T`) are refused by name in front2's heads.rs. Owed: `polarity: Positive |
  Negative` on `ImplHeader`, with all its users in one batch, and the solver refusing an
  auto-trait goal a negative impl covers. First check whether `*const T: Send` is answered
  structurally as yes today: that would be a wrong answer.

(10) Runner after (9) (2026-11-16):
- Reproduced on the real front end: crates/blaze-ladder/tests/front2_panics.rs
  (`panics_through_front2`: `panic!` with captures, `if false { abort() }` then `panic!`, a
  `panic!` arm, `panic!()`, `assert!`, two `Display::fmt`, a slice index, and core's
  `panic_bounds_check` found by its lang name). First run: "core::panicking::panic_bounds_check:
  UnknownInCleanBody at ExprId(6): Ok(Lit(Str))" (the template string), and the same at
  ExprId(1) `"{}"` in `write!`. The root was blaze-lower machine.rs FORMAT_ARGS_EXPR, which left
  the template literal in the body with no list holding it. The expansion lane fixed it there
  (the slot becomes `Expr::Missing`). The checker now refuses such a node by name:
  `Malformed::Unreached(ExprId)`, from a `reached` column the walk fills in `finish_expr`
  (suite: `an_expression_no_list_holds_is_refused_by_name`; without it: "Err(UnknownInCleanBody
  { at: ExprTy(ExprId(0)) })").
- A placeless mismatch can no longer happen: `coerce_many_push(site, Source, ty)` takes
  `Source::Expr(e)` or `Source::Unit(of)`, where `of` is the block, `break` or `return` the
  forced `()` stands for. `At::Body` is gone from that path (suite:
  `a_forced_unit_mismatch_carries_the_blocks_range`; without it: "DiagWithoutPlace { code:
  Mismatch, name: None }").
- In a user crate, `panic!()` and `assert!` give E0425-like `UnresolvedPath` at the macro
  call. The `panic!` arms' mismatch comes from the `panic_fmt(..);` statement not diverging, and
  a path that does not resolve would explain both: `$crate::panicking::..` from a user crate.
  The test prints each diagnostic's name hash beside `Name::of` of the expansion's words, to
  say which segment fails.
- Negative impls: the solver already had `ImplPolarity` (trait_goals.rs `step_impl`,
  structural.rs's disqualification by any impl of the shape). The world's `ImplHeader` now has
  `polarity: ImplPolarity` (the solver's type, re-exported). infer/facts.rs passes it through
  instead of answering `Positive`, and the checker's own impl selection skips non-positive
  impls (instance.rs `select`, machine.rs `leaf_clause_of`, infer/mod.rs). front.rs,
  world_items.rs and blaze-build's tests answer Positive. front2's heads must fill it from the
  `!` (the heads lane was told). Suite: `a_negative_impl_keeps_an_auto_trait_from_holding`.
  Until the heads fill it, `*const T: Send` IS answered yes structurally on front2 (no impl
  in the bucket, so the type's parts prove it).

## Closures against `Fn` bounds (2026-11-16, closures lane)

The library's definitions (library/core/src/ops/function.rs): `#[lang = "fn_once"] pub trait
FnOnce<Args: Tuple> { #[lang = "fn_once_output"] type Output; extern "rust-call" fn
call_once(self, args: Args) -> Self::Output; }`, `pub trait FnMut<Args: Tuple>: FnOnce<Args>`,
`pub trait Fn<Args: Tuple>: FnMut<Args>`. The Reference ("Closure types") says a closure type
implements `FnOnce`, and `FnMut`/`Fn` by how it uses its captures; it is silent on how the
closure's signature is deduced from the callee's bound. blaze's rules:

- **Signature from the pending bound.** A closure checked against an expectation that is an
  inference variable `?F` takes its parameter types from a pending `?F: Fn*<Args>` (`Args` a
  tuple: one type per parameter) and its return type from a pending `<?F as
  FnOnce<Args>>::Output == R`; `R` may itself be a variable the body then decides (`then(||
  k())` decides `T` of `then<T, F: FnOnce() -> T>`). Reason: the call checks closure arguments
  last, after the other arguments and the expected output have constrained `Args` and `R`, so
  the bound is the one source that knows the parameter types; a written annotation still wins.
- **The output projection.** `<C as FnOnce<A>>::Output` for a closure, fn item or fn pointer
  `C` normalizes to its signature's output when `A` unifies with its inputs tupled; a
  closure's kind (unknown until its captures are known) never blocks `FnOnce`, only `FnMut`
  and `Fn`.
- **An associated-type lang item is named from its trait.** The solver names every
  associated type `assoc_def(trait, name)` (infer/facts.rs). The four lang items that are
  associated types (`fn_once_output`, `future_output`, `discriminant_type`, `metadata_type`)
  are therefore answered by the checker from their trait's lang item and their declared name
  (`FnOnce::Output`, `Future::Output`, `DiscriminantKind::Discriminant`, `Pointee::Metadata`:
  `LangItem::assoc_type`), confirmed to exist by `assoc_item(trait, name)` (a typed
  `Malformed::NoRecord` if not), and never by the world's key of the item itself. Reason: the
  real world answers `lang_item(FnOnceOutput)` with the `type Output` item's own key, which is
  no `assoc_def`, so `is_lang(item, FnOnceOutput)` never held and every closure's output
  projection was E0277 (census-bnd-1: 132 rows, `<Closure(..) as FnOnce<()>>::Name(..) ==
  AudioIn`); one name per associated type makes that mismatch impossible.

Built: `LangItem::assoc_type` (ids.rs) and the `SQ::Lang` fill (infer/facts.rs); the fixture
worlds no longer answer the four items themselves. Fixtures: crates/blaze-typeck/src/suite/
closures.rs (zero- and one-argument closures deciding a callee's parameter, by fn call and by
method call, capturing or not; a fn item, a variant constructor and a fn pointer for `F:
FnOnce(E) -> O`; a closure returned by a closure; a stored closure called directly, as `&f`
and `&mut f` through the library's reference impls, and through `let r = &f`). Without the
fill change all five closure/fn-item cases fail with `diagnostics: want [], got
[(Unsatisfied, 0)]`. The steps of the signature rule above needed no change: the pending-bound
deduction (`deduce_sig`, `closure_hint`) and `normalize_fn_once_output` were right; only the
lang item's name was not. The census's `FnOnce<(KvError)>` is a one-tuple: check.rs's printer
writes a one-tuple without its comma.

## Not decided here

- Whether a run stopped at `Need` should save its state instead of re-entering: decided by a
  profile of re-entries on reth and tempo.
- Regions: typeck erases them as rustc does; the borrow checker keeps its own.
