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
- After both fixes: "clean: [\"e\", \"core::panicking::panic_bounds_check\", \"<impl
  core::fmt::Display for W>::fmt\", \"<impl core::fmt::Display for V>::fmt\"]". OPEN, and not
  the checker's: in a USER crate, `$crate::panicking::panic_fmt` (name hash 8263167416644613366
  = `Name::of("panic_fmt")`, printed by the test) and `panic` (`panic!()`, hash
  12722707682089970518) answer `UnresolvedPath` at the macro call. The `panic!` arms' E0308
  (now at the call's own range, 38..89) follows from it: the unresolved call is not `!`. Next:
  find where front2 or the names library resolves a `$crate` path out of a user crate's
  expansion. Bodies a, a2, b, c, d of front2_panics.rs fail on it.
- Negative impls: the solver already had `ImplPolarity` (trait_goals.rs `step_impl`,
  structural.rs's disqualification by any impl of the shape). The world's `ImplHeader` now has
  `polarity: ImplPolarity` (the solver's type, re-exported). infer/facts.rs passes it through
  instead of answering `Positive`, and the checker's own impl selection skips non-positive
  impls (instance.rs `select`, machine.rs `leaf_clause_of`, infer/mod.rs). front.rs,
  world_items.rs and blaze-build's tests answer Positive. front2's heads must fill it from the
  `!` (the heads lane was told). Suite: `a_negative_impl_keeps_an_auto_trait_from_holding`.
  Until the heads fill it, `*const T: Send` IS answered yes structurally on front2 (no impl
  in the bucket, so the type's parts prove it).

(11) Runner after (10) (2026-11-16):
- "diag E0277: Clone" REDUCED on the real front end: crates/blaze-ladder/tests/front2_clone.rs
  (`clones_through_front2`): `Arc<T>` by method and by path (the `Global` default filled,
  `Global: AllocatorClone` proven), `Arc<T, A>` from `A: AllocatorClone`, `A: Clone` through
  that supertrait, `Arc<[u8]>`, a derived generic enum at a concrete type, a derive written by
  a `macro_rules!` expansion (bstd's `typed_name!`), and flight's shape (`Tb::new_in` asking
  `H: Clone` while `H` is a variable a struct literal fixes later). ALL CLEAN: "test
  blaze-ladder: exit 0; 1 passed, 0 failed". So no rule of the checker is missing for these.
  The census itself says why: "from empty records: 1 of 3 examples the same; else [\"clean\",
  \"clean\"]": the device and grant bodies are clean when checked first: ORDER DEPENDENCE (the
  THIRD lane's class, front2_order.rs). The one that stays, flight's `planted`, differs from
  the reduction only in its `Arc` being `blaze_own::Arc` (`impl<T: ?Sized, A: AllocatorClone>
  Clone for Arc<T, A>`, crates/blaze-own/src/arc.rs:477): a user crate in the test cannot name
  blaze_own (no dep), so the next step is a census run of just that body from empty records,
  then whether blaze_own's impl reaches the bucket (the heads lane's if missing).
- E0283 names its goal: `report_fulfill_errors` gives an ambiguous trait or projection goal
  `unmet: Some((goal, None))` as E0277 does (suite `an_ambiguity_names_its_goal`; the
  without-fix line was NOT run: owed). front2/check.rs's printer no longer says "(a projection
  goal is not named by the checker yet)": every goal's finding carries `Unmet`; what is left
  (E0599, E0308, E0609, E0425, ..) is about a node and prints its types and looked-up name.
  Still unnamed: a failed `defer`red projection (its obligation's goal is `WellFormed`).
- `Pointee::Metadata` normalized: alias.rs `Rule::Metadata`, builtin.rs `metadata_ty` (a loop
  to the struct or tuple tail; `usize` for `str`/slices, `DynMetadata<dyn ..>` for an object,
  `()` otherwise; a parameter or placeholder tail `()` when the env has its `Sized`, else the
  rigid `<tail as Pointee>::Metadata`). OPEN: an ALIAS tail is answered rigid even when its
  item bounds say `Sized` (rustc probes a `Sized` goal there; the solver has no in-step probe).
  Fixtures: builtin.solve `metadata_*` (nine); the test world now reads `env T: ?Sized`.
  Without the candidate: "solve cases: 287 cases, 312 goals, 304 passed, 8 failed".
- `panic!` in a user crate: still `UnresolvedPath` `panic_fmt` (Name 8263167416644613366);
  clean: "e", "core::panicking::panic_bounds_check", both `Display::fmt`. a2 now also has an
  UnresolvedPath at 50..73 (Name 4032013252565412311: `core::intrinsics::abort`'s segment?).
- NOT STARTED: step 5 (`{ None }` for an `Option<u8>` const parameter: `variant_value` makes
  `Adt(enum, [])`, without the enum's arguments; then E0283 in bstd::kv::<impl Kv<P>>::get).

(12) Runner after (11) (2026-11-16):
- tempo's "347 diag E0308" is NOT a method-probe bug and not the checker's: alloy `sol!` writes
  a no-parameter function or error as `#[derive(Clone)] pub struct MCall;` (alloy-sol-macro-
  expander 1.7.3 expand/function.rs `pub struct #call_name;`, expand/error.rs), and blaze-
  expand's derive writes `match self { MCall => MCall, }`. blaze-lower machine.rs:2154 makes
  the pattern `Pat::Bind { candidate: true }`; no step reads `candidate`, and the arm's
  `MCall` resolves to that binding (the Reference, "Identifier patterns": "Path patterns take
  precedence over identifier patterns"). Reduction: front2_clone.rs
  `unit_struct_patterns_through_front2` (#[ignore]d, owner named): "by_hand: diagnostics
  [(Mismatch, Range { start: 52, end: 57 }, \"found Some(\\\"&MCall\\\"), expected
  Some(\\\"MCall\\\")\")]", the same for the derived `clone`s of `MCall` and `NCall`; the
  enum's derived `clone` (`Clone::clone(__self_0)`, a path call: no probe) is clean. Owner:
  blaze-lower's scoped resolution (the items-inside-bodies lane handed on; told the
  coordinator). The fix needs the world (is the name a unit struct, unit variant or const?),
  so the decision belongs in resolve.rs, re-pointing the arm's uses from the binding to the path.
- The probe rule asked for ("a candidate whose applicability is unknown stops the probe") holds
  by construction, so nothing was added: `instantiate_method` reaches the world through
  `impl_header`/`fn_sig` (`Fetch` `Missing` becomes `Stop` through `?`) and `may_hold`, whose
  `evaluate` fills a `Need` from the world or stops; `Answer::Maybe` counts as applying (picked,
  or E0034 with another). A candidate is passed over only on `None` (no such item) or
  `Answer::No`. No fixture: there is no rule to remove. CHANGING: the speed lane
  (a654be8e86b3bc929) is making a world `Missing` a HOLE: `Reads` returns a stand-in (`None`,
  `&[]`) and records the question, so the probe WILL pass over a candidate on a hole; it stays
  sound only because a run that met any hole returns `Outcome::Need` and no result (lib.rs
  `run`). Keep that invariant: no diagnostic may leave a run with a hole.
- Return-position `impl Trait`, the checker's side: the solver already instantiates an opaque's
  bounds at the alias's arguments (`bound_pred` then `import_pred(portable, p, largs)`, alias.rs
  :701); `OpaqueDef.generics` is read by no opaque path (grep), so the hard-coded 0 in facts.rs
  is harmless as long as the front end numbers the bounds' parameters as the identity list.
  Fixture world: `Fx::opaque_key`/`set_opaque_bounds`; suite/opaque.rs (a generic `mk<T>() ->
  impl Tr<Out = T>`: the caller proves `Tr` and normalizes `Out` to `u8`; a `u16` return is
  E0308). NOT RUN: blaze-typeck did not compile under the closure-identity lane's open batch
  (`PNode::Closure { .. }`, 15 errors in its files). Run `lanecheck.sh blaze-typeck test opaque`
  first. Not built: the defining body (hidden type = the returned expression's type, which must
  satisfy the bounds), auto-trait leakage (structural.rs asks `hidden`; `None` is ambiguous).
  The types-and-bounds lane (a5cc7587e39092659) BUILT the world side, uncommitted:
  front2/keys.rs `opaque_key(fn_key, index)` (preorder over the return type; an async fn's
  `impl Future` is 0), `Opaque(k, L)` with L = Param/ConstParam over parent+own incl. APIT,
  `predicates(k)` = bounds + Sized + bindings as projections (async: `Future`, `Output == T`),
  `opaque_hidden` = Ok(None). Its fixtures (front2_bounds.rs) were clean before the closure
  batch. TWO for this lane: (a) `match opt_iter() { Some(mut i) => i.next(), None => None }`
  (an opaque inside `Option`, then a method) gives `DiagWithoutPlace { code: Unsatisfied }`
  while `let mut i = evens(); i.next()` is clean; (b) the defining body: an async fn's body is
  lowered as a plain block (blaze-lower desugar.rs:280), so the checker must read `sig::ASYNC`
  (tail type = the future's `Output`), and a `-> impl Tr` body must check its returned expr
  against the bounds.
- Head census questions: (1) `u24`/`i24`/`u40` (core num/arbitrary.rs): `IntTy`/`UintTy` hold
  the standard widths only; the type needs a width-carrying form (`Int { signed, bits }`) in
  ids.rs, portable.rs and blaze-ty, literal range checks by bits, casts by bits, and the
  operator impls are the library's: one batch with the code builder. Not built. (2) `T::X<A>`
  (a GAT with type arguments): the projection term is `Alias(trait, args)`: the associated
  type's own arguments go after the trait's (rustc's `AliasTy` args are the trait's then the
  item's own), and `AssocTy.own_generics` (facts.rs, 0 today) says how many. Not built.
- bstd census-gen-3: "6 diag E0308" (`None` against `Option<Arc<Budget>>`, `Ok(None)` found
  `Error`) and "7 diag E0283" (`Error: Rights` at `Self::refuse`) look like (11)'s step 5
  (`variant_value` without the enum's arguments): unproven. "13 diag E0599": `P::WRITE` (an
  associated const through a parameter's bound) and `self.code.hash(state)` on `ContentId`.
  Not reduced.

(13) Runner after (12) (2026-11-16), tempo's checker classes (tempo-front-4: 916 E0034, 611
E0599, 151 E0277):
- ONE home for `T::name` as a value: lit_path.rs `assoc_value`. Inherent first (an applicable
  inherent item ends the search, already so), an item listed twice by `World::inherent` counts
  ONCE (by item key; no order assumed), the trait list deduplicated, and the trait of the impl
  the body is a member of joins the traits in scope (`assoc_parent(owner)` then
  `impl_header(container).trait_`: rustc_resolve's `traits_in_scope` adds `current_trait_ref`).
- `Self::A` / `Alias::A` where the prefix is `Def::Adt` of an enum (front2 world_paths.rs answers
  `Self` as `Def::Adt` consuming ONE segment): `value_path` takes a variant of that name before
  any associated item (rustc's `associated_path_to_ty`), in expressions and patterns (both go
  through `value_path`); the ctor's type is one helper, `ctor_ty`, shared with `Def::Ctor`.
- Proof: blaze-typeck suite/self_paths.rs (fixture answers as front2 does: `Self` is
  `Def::Adt`, one segment): with the fix "test blaze-typeck: exit 0; 2 passed, 0 failed";
  without the two rules: "diagnostics: want [], got [(NoMethod, 0)]" for both
  (`a_variant_through_self_is_the_variants_ctor`, `self_reaches_the_impls_own_trait_without_an_import`).
- NOT RUN: blaze-ladder tests/front2_selfpaths.rs (A1-A3, B1-B5, C1-C9 through the real front end,
  incl. alloy's exact `type RustType = Self; type Token<'a> = <U as SolType>::Token<'a>;`).
  Every run was blocked by other lanes' open batches (last: "crates/blaze-consteval/src/
  machine/code.rs:764:19: error[E0061]: this function takes 6 arguments but 5 arguments were
  supplied": `blaze_build::ground`). RUN IT FIRST. Expected leads: (A) the duplicate is the
  front end's: buckets.rs `answer_inherent` pushes into `inherent` and can fail mid-loop on
  `Unfilled::Fn(f)` (`?`), so the retried fill pushes the impl's earlier items again: if the
  reduction is clean, that is the census's 916 (the checker now counts them once; the bucket
  should still be made idempotent: ITEMS lane). (C) solve alias.rs `step_impl`: an impl whose
  `h.assoc` lacks the item gives `TyKind::Error` ("== Error" in the census's SignatureType
  example): suspect the front end dropping the impl's associated types when one is a GAT whose
  value is a GAT projection (`Token<'a>`; facts.rs `own_generics: 0`); c7/c8/c9 tell which.
- Suites: blaze-typeck "115 passed, 5 failed" before the fixture (3 closures: the closure batch's
  open work; 2 suite/opaque.rs: owed from (12), plain paths only, untouched by this change);
  blaze-solve "8 passed, 0 failed". suite/opaque.rs is now the opaque lane's (addae433814bf7ee9:
  its two fixtures pass with the heads' `Sized` bound).
- OPEN, a panic (the integrator, a14c41bba58ed36a1, 11:40: ladder_front2 and front2_main):
  "crates/blaze-ty/src/infer.rs:247:23: index out of bounds: the len is 31 but the index is 31"
  from machine.rs `failed_leaf` -> `leaf_clause_of` -> unify. `leaf_clause_of` returns a
  where-clause whose args come from `resolve` over the impl's fresh arguments made INSIDE
  `failed_leaf`'s snapshot; `rollback_to` deletes those variables, and the next round unifies
  against them. Fix: return `Some(None)` when a resolved argument still holds an inference
  variable (needs a small "has inference variables" query on `TypeCtx`), with a fixture whose
  failed goal's impl leaves a parameter unfixed by its header.

(14) Runner after (13) (2026-11-16), `T::NAME` on a type parameter (core's `T::IS_ZST`, bstd's
`P::WRITE`) and what the reductions said. Handing on.
- RUN: tests/front2_selfpaths.rs as (13) left it: "test blaze-ladder: exit 0; 1 passed, 0
  failed": A1..B5 and C1..C9 all check clean on the real front end. So the projection class
  (tempo-front-5 "81 diag E0277 `<InboundKind as SolType>::RustType == InboundKind`") is NOT a
  generic associated type beside `type RustType = Self;` (C7..C9 pass). Unproven lead: alloy
  writes `impl SolType for X` inside `const _: () = { .. }`, the impls-inside-blocks lane's
  class; a reduction with the impl inside a block was not run.
- NEW tests/front2_params.rs (eleven bodies). Before the change every body, `P::make()` too:
  "p1: Gap { .. gaps: [RefusedImpl { impl_: ItemKey(b6e412072608), place: \"(no file place)\",
  why: .. argument 2 of `HasField` is a constant; that parameter is not a const parameter".
  Cause, the checker's: `assoc_value` asked `inherent(Shape::Param, name)`, whose answer lists
  every inherent impl the front end could not read by shape. FIXED in rules/lit_path.rs: a
  parameter has no inherent items and the world is not asked; after: "p4: clean".
- ALSO in `assoc_value`: two tiers, a parameter's bounds before the traits in scope (fixture
  suite/self_paths.rs `a_parameters_bound_comes_before_the_traits_in_scope`; with the tiers
  merged it says "want bool, got {error}; diagnostics: want [], got [(AmbiguousMethod, 0)]").
  Four new fixtures (`Fx::trait_const` is new): a bound's constant, a blanket impl's, bounds
  first, two bounds E0034.
- THE FRONT END'S, open: every body naming a TRAIT's constant says "p1: diagnostics
  [(NoMethod, Range { start: 27, end: 36 }, \"found Some(\\\"Param(0)\\\"), expected None ..",
  the census's line. front2/world_items.rs `assoc_item` for a trait reads `t.fns` and
  `extra.assoc` only; heads.rs `TraitHead` has no constants (a `Const` member is only
  `DynMember::Const`). Needed: a member key and signature row per trait constant (as an
  impl's `consts`) and `AssocKind::Const` from `assoc_item`. Probably also tempo-front-5's
  "55 NEW diag E0576" at `<X as SolCall>::SELECTOR` (not reduced). The names in the test's
  `OWED` list assert today's line and must be removed as the answer arrives.
- The checker and solver already hold `Q: Zst` through `impl<T> Zst for T` (fixture
  `a_parameters_constant_comes_from_a_blanket_impl` passes): once the front end answers a
  trait's constants, `T::IS_ZST` needs nothing more from the checker that is known today.
  The fixture world answers a trait constant where the real world does not yet: the fixtures
  are ahead of the front end here, by intent, and front2_params.rs is the real measure.
- NOT STARTED: (D) `variant_value`, "Sort { node: 128 }", `S::View<'_, T>`, `u24`; bstd's
  `self.code.hash(state)` on `ContentId`.

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

## A closure's identity (2026-11-16, closure identity lane)

The Reference ("Closure types"): "A closure expression produces a closure value with a unique,
anonymous type that cannot be written out"; the `Fn*` traits it implements follow from how it
uses its captures ("Capture modes"); a closure in a generic item is generic over the item's
parameters. blaze's portable form is `PNode::Closure { owner, ordinal, args }`:

- `owner`: the `ItemKey` of the item whose body holds the closure expression.
- `ordinal`: the closure's index among the body's `Expr::Closure`s in expression order (the
  lowering's order, a function of the body's text alone). An edit above it may shift it; the
  owner's key is from the body's text, so that edit changes the key and every record holding
  the old ordinal is keyed by the old text anyway. Never an `ExprId` in a shared record.
- `args`: the owner's generic arguments in scope (`Param(0..n)` in its own record), so `f::<u8>`
  and `f::<u16>` make two closure types; instantiation substitutes them like any argument.

The signature, the kind (`Fn`, `FnMut`, `FnOnce`: the strongest it implements) and the captures
(each a binding and a mode: by shared reference, by mutable reference, by value) are not the
type's identity: they are facts, decided when the owner's body is checked and stored in its
`Checked::closures` (one row per closure, by ordinal: the expression, the kind, the signature as
a `FnPtr`; the captures in `Checked::captures`). Another body or an instance that meets the type
asks the world `Question::Closure(owner, ordinal)` (a body record, as an opaque's hidden type) and
gets `ClosureFacts { kind, sig, captures, types }`, or a typed gap when the owner is unchecked.
The checker lowers a foreign closure to `TyView::ClosureOf { owner, ordinal, args, sig, kind }`,
its signature substituted by `args`; the solver sees rustc's layout `[args.., kind, sig, ()]`,
so its `Fn*` rules read the signature and the kind from the type with no change. Inside the
owner's body nothing changes: a closure is `TyView::Closure(expr, sig)` while its signature is
inferred, and writeback names it by ordinal.

Counting (decided 2026-11-16): the owner is always the ITEM; every `Expr::Closure` of its body,
nested ones included (a closure inside a closure is the item's too, not the outer closure's), is
numbered in `ExprId` order, which is the lowering's order of the text. An edit inside one closure
can renumber the closures after it; that is harmless, because any edit in the body changes the
owner's body key, so no record keyed by the old text survives it. `move` comes from the lowering's
`Closure::capture_by_value` (blaze-lower already records it) through `tc::Expr::Closure::is_move`.

The sites of the one batch (found by search, 2026-11-16): blaze-typeck: portable.rs (the variant,
`node_children`, `hash_node`), ty.rs (`TyView::ClosureOf`), columns.rs (`Checked::closures`), world.rs
(`Question::Closure`, `World::closure`, `Reads::closure`, `ClosureFacts`), asks.rs (kind 18),
machine.rs (`lower_node`, `children`, `shape`), infer/mod.rs (`mk`, `view`), infer/facts.rs
(`convert` refuses), instance.rs (`import` through the question, `export`, `shape_of`),
rules/writeback.rs (export by ordinal, the closures table), rules/closure.rs (`ByValue`, `move`),
rules/call.rs (a direct call of a foreign closure), body.rs (`is_move`), fixture.rs (`Fx.closures`,
`World::closure`, `show`), toy.rs; blaze-lower typeck.rs (`is_move`); blaze-build instance.rs,
closure.rs, walk.rs, layout.rs, tests.rs (only what must compile); blaze-ladder front2 types.rs,
check.rs, buckets.rs, world_items.rs (`closure`: a `Gap::BodyRecord`), front.rs, run.rs; blaze-real
examples/check.rs; blaze-flow-lower lib.rs.

The kind and the capture modes are two facts ("Call traits and coercions": "`move` closures may
still implement `Fn` or `FnMut`"). A capture's MODE is `ByValue` for every capture of a `move`
closure and for a non-`Copy` value used by value, else `ByMutRef` for a mutable use, else `ByRef`;
the closure's KIND is the strongest USE (moved out: `FnOnce`; mutable: `FnMut`; else `Fn`),
recorded in `Checked::closures` beside the exported signature. Tests: suite/closure_identity.rs
(two bodies through `F: Fn(u8) -> u8` and a direct call; the adaptor `Map<F>` with `(self.f)(x)`;
`gen::<u8>` and `gen::<u16>` as two types; the five kinds and captures incl. `move || x` with `x:
Copy` = `ByValue` + `Fn`). Without the owner's facts the caller stops with
`Malformed(BodyTypeInRecord(Closure(..)))` (run 2026-11-16).

Not in yet:
- A solver-rebuilt term of a foreign closure is viewed by matching its def and its owner's
  arguments after a SHALLOW probe (`foreign_resolved`); a variable nested in an argument
  (`gen::<Vec<?T>>`), or a term the solver made by substitution alone, panics ("a closure this
  context did not make").
- Upvars are `()` for a foreign closure: the solver sees no capture types, so `Copy`/`Clone`/auto
  traits of another body's closure are not decided by its captures. `ClosureFacts` would need the
  capture types.
- A closure in a const or static owner gets 0 arguments (`own_params` comes from a fn sig only),
  wrong for a const inside a generic impl.
- Moving out through a field or an index (`consume(s.f)`) is not told apart: the base's use is 0,
  so such a capture is `ByRef`/`Fn`.

## Overloaded deref and `?` on the real world (2026-11-16, second checker lane)

The rules (the Reference: "Method call expressions", "Field access expressions", "Type
coercions", the dereference operator, "The question mark operator") were already built as the
Reference states them: `autoderef` (machine.rs) is a bounded loop of builtin and overloaded
steps (`overloaded_deref`: `T: Deref` may hold, then `<T as Deref>::Target` normalized), shared
by method probing (by value, `&`, `&mut` at each step, the array-to-slice step last), field
access, the borrowed-pointer coercion and the unary `*`; `?` is lowered to `Try::branch` and
`FromResidual::from_residual`. None of them needed a change.

The census's five classes (E0599 112, E0308 41, E0609 31, E0614 19, E0277 `Try` 81) were one
cause outside these rules: `coherence::fresh_for` (blaze-solve) took each impl parameter's kind
from the whole shared table, so `impl<T, A: Allocator> Deref for Vec<T, A>` or `impl<T, E> Try
for Result<T, E>` got a const variable for a type parameter once any item with a const
parameter at that index had been read, and the impl never matched; whether it did depended on
what was checked before (the third checker lane's fix, 10:07). The proof is the real world:
crates/blaze-ladder/tests/front2_checks.rs checks 29 bodies clean through the real front end and
toolchain (`Vec`, `Box`, `Rc`, `Arc` through `Deref`, one to three steps, `DerefMut` for a
`&mut self` method and `*g = 1`, deref coercions at an argument and at a typed `let`, `?` on
`Result` with and without a `From` conversion and on `Option`), `vec_iter` and `try_same` first
of all, and six bodies that must be refused with their finding (a deref's target is the impl's
`Target`, not a variable the context picks; no `From` for the error; `Option`'s residual in a
`Result` fn). Before the fix eleven of its bodies failed with the census's findings.

## Items inside bodies (2026-11-16)

The Reference, "Statements", "Item declarations" (fetched 2026-11-16): "Declaring an item within a
statement block restricts its scope to the block containing the statement. The item is not given a
canonical path nor are any sub-items it may declare." "The exception to this is that associated
items defined by implementations are still accessible in outer scopes ..." "There is no implicit
capture of the containing function's generic parameters, parameters, and local variables."

**The cause, proven.** crates/blaze-ladder/tests/front2_locals.rs checks bodies through the real
front end. Before any fix every local-item body fails, each with the one finding the census counts:
`local_fn: diagnostics [(UnresolvedPath, Range { start: 45, end: 50 }, ..)]`, the same for the call
before the definition, a local `const`, a local struct with an inherent impl, a local `use`, and
`local_macro` (the pinned core's `assert_unsafe_precondition!` shape: ub_checks.rs line 68 declares
`const fn precondition_check($($name:$ty),*)` inside the expansion's block, line 78 calls it);
`local_shadows` checks against the MODULE's `outer` (`Mismatch .. found Bool, expected u8`), which
shows the module is asked before, and instead of, the block. The expansion lane's reading is right.

**Who knows what, today.** The lowering already does its half: blaze-lower machine.rs
`stmt_list` makes one `LocalItem` row (syntax kind, name, item-relative range, block scope) per item
statement, macro-made ones included, and `Scope { parent, items }` per block; blaze-lower
resolve.rs `local_item` walks that chain innermost first. front2 never reads them:
world_items.rs `BodyWorld::resolve_path` drops its `ScopeId` and world_paths.rs `BodyWorld::path`
asks only the names library at the body's module.

**The design.**
1. Identity (new file front2/locals.rs, this lane's): a local item's library key is
   `blaze_scope::task::key_of(crate, enclosing_body_key, kind_tag, name, ordinal)` with
   `ItemKey::INNER` set: the names library's own function for members (lower.rs line 1744), the
   ordinal counted among the same (kind, name) in the enclosing body's `LocalItem` rows, which are in
   text order: no dependence on the order bodies are checked in. Filed in `Keys` as `Origin::Item`.
2. Resolution (world_paths.rs `path`, heads-ITEMS lane's file): `path` takes the `ScopeId`; the
   first segment is looked up in the body's local items through the scope chain, innermost first
   (one bounded loop, as blaze-lower's `local_item`), in the path's namespace, BEFORE `Self`-less
   module lookup; nothing found: the names library at the module, unchanged. A local `use` is a row
   whose tree is resolved at the module (paths inside it may name local items: same chain).
3. Head (heads.rs `Store::step`, heads-ITEMS lane): `step` reads (detail, text, module) from the
   names library; a local item's come from the store's local table instead. THE OPEN POINT: heads
   borrow their text for `'n`, but a macro-made local item's text lives in the enclosing body's
   lowering record (`Columns::expanded`), which `Body` owns and drops. Either the store copies the
   item's text into an arena of its own lifetime, or heads stop borrowing text: the heads lane's
   call. Its module is the enclosing fn's; types in its signature resolve through the same chain.
4. Body: a local fn's body is lowered and checked as its own body (its own params, no outer
   generics, by the Reference), on demand like any fn's.
5. Impls: a local `impl` is visible crate-wide (associated-scope rule): the impl index learns of it
   only when its enclosing body is lowered; until a crate-wide pass lists every body's local impls
   (the names library's outline marks `body_holds_items`, the pass's candidates), a method found
   only through a local impl from another body is a typed gap naming it.
6. Forms not reached: `macro_rules!` in a block (textual scope, handled by the lowering's
   expansion), a local `trait`, `static`, `union`: each a typed refusal naming the form.

**Built.** The fixture (all failing, as quoted). **Next.** Items 1 to 3 for `fn` and `const` (the
core macro's case, the 74 E0425 of census-gen-3 and the 275 of the owner's walk), then `use`,
`struct`/`enum` with inherent impls, then item 5.

**Runner 2: what is built (2026-11-16, ~11:40; handing on, NOT YET RUN).** Written, compiled
once for step 1 only (mixcheck "check ok (all targets) at 11:16; tree d3616b8a88"); steps 2 to 3
are written but never compiled: every later build stopped in other lanes' mid-edits (buckets.rs
`Home::admits`, check.rs `add_solver_counts` against blaze-typeck, blaze-typeck's closure
`PNode::Closure`). The next runner's FIRST call: `lanecheck.sh blaze-ladder test "--test
front2_locals"` (or mixcheck), fix what is mine, quote the line.
- THE TEXT (coordinator's decision): `Front2::new(names, frame, user)` takes the caller's
  `&'n blaze_mem::Frame` (kept as `front.frame`); `Store::new(sized, frame)`; `Store.locals`
  (front2/locals.rs `Locals<'n>`, its maps and slices in the frame). `Front2Program::new` and
  `survey` take the frame too; run.rs makes it. Every caller updated (tests, examples,
  tempo_front leaks one for 'static). `Names<'n>` is invariant: a frame made in an inner block
  does not live long enough; front2_main's reversed run reuses `front.frame`.
- body.rs `lower_at` calls `store.locals.keep(..)` after a clean lowering: a body with no
  LocalItem row keeps nothing; else each row: kind (syntax), name, item-relative site, text (a
  slice of the fn's own text, or a frame copy where an expansion wrote it), key
  `key_of(crate, key_of(crate, item, "Body", hex(owner), 0), tag, name, ordinal)` with INNER set,
  filed in `Keys`; a `use` row per leaf of `blaze_scope::task::use_leaves` (a glob in a block
  is refused by name). `lower_fn_into` reads a local fn's text/module from `store.locals`.
- LOOKUP `Locals::find(owner, scope, name, ns)`: the block chain innermost first, then (for a
  local item's own body) the blocks around its declaration; bounded loops, no recursion.
  world_paths.rs `BodyWorld::path(scope, ..)` asks it after `Self` and the generics, before the
  module: fn/const/static answer with the derived key (a fill of `Want::Fn` first if the head
  is not made); a local `use` rewrites the path to its target and resolves at the module; any
  other form (struct, enum, impl, trait, mod, alias) is `Gap::Local(LocalGap{item, form,
  range})`. `BodyWorld.owner` and `BodyScope.owner` carry the body's key; reach.rs passes
  `bp.scope`.
- HEADS: heads.rs `Store::step` takes (detail, text, module) from `locals.head_of` first (fn,
  const, static; other kinds an `Unsupported(form, range)` refusal), so the head is made by the
  same `fn_head` from the frame's text, in the enclosing module with no outer generics.
- Census prints "LOCALS: bodies that kept their local items (shared store): N".
- NOT DONE: steps 3's local structs and impls (gap, not built: `adt` reads variants and ctors
  from the names library), the census and walk numbers, `front2_order` fixtures, front2_panics.

## Impls inside blocks (2026-11-16)

The Reference, "Statements", "Item declarations": "An item declaration statement has a syntactic
form identical to an item declaration within a module. Declaring an item within a statement block
restricts its scope to the block containing the statement." An implementation is not a name: it
holds for its type in the whole program (coherence is per crate), and only its header and members
are READ in the block's scope.

**The cause, proven** (crates/blaze-ladder/tests/front2_nested.rs, before any change): "NESTED
impls in the index: 0"; `in_const: diagnostics [(NoMethod, Range { start: 26, end: 31 }, "found
Some(\"X\") ..), (NoAssocItem, Range { start: 34, end: 46 }, ..)]`; serde's shape
`block_extern_crate: [(Unsatisfied, .. "`Xd: ArbitraryInt` because `Xd: Default`")]`: a wrong NO
on eight correct bodies (the census's E0599, E0576 and E0277).

**The design (written before the code).**
1. The scan is syntax's: `blaze_file_items::nested::scan(text, alloc)` (new file nested.rs) reads
   ONE body-like text (a fn, const or static item's whole text) by tokens, with a stack of open
   brackets and no recursion, and returns its blocks (each with its parent) and the item
   statements in them (kind word, range relative to the text, attributes included), plus the macro
   calls in statement position (which may write items: not expanded here). An `impl` inside a
   string, a comment or a macro call's arguments is one token or inside a non-block bracket: never
   counted. It is a pure function of the item's text: an edit to one body rescans that body.
2. Its home in the pipeline is the NAMES LIBRARY, not front2 (where the tree's shape forces a
   difference from the coordinator's sketch): every consumer of an impl (front2's `Index`,
   `Buckets::file`, `Store::step`, `names.members`, the census's body list) reads the library's
   tables by key, so a nested impl that is in `tables.impls`, `texts` and `members` needs no second
   path in any of them. blaze-scope resolve.rs `make_blocks` runs the scan on each lowered item and
   member whose outline flag says `body_holds_items` and whose text holds the word `impl`.
3. THE BLOCK IS A SCOPE RECORD: for a body with an impl, each block on the way to an impl becomes
   an unnamed child module of the item's module (a "made" module, as a macro-written `mod` is):
   its declarations are the block's own item statements (lowered from their own text by the one
   lowering) and one glob import of the scope outside it. So `use x as y;`, `extern crate s as
   _s;`, a local struct, the blocks around and then the module are found by the ONE name solver
   (explicit declarations shadow the glob: innermost first), and a head resolves in a block
   exactly as in a module: no second lookup rule. The enclosing fn's generics and locals are not
   items, so they are not in scope (the Reference: no capture).
4. Identity: the block fragment's key is `key_of(crate, enclosing item, "blocks", "", 0)` (a
   names-library key like a macro call's: `INNER` is not set, the lowering's `make_key` clears
   it); a block module's is `(fragment or outer block, "mod", "__blaze_block_N")`, N its ordinal
   among the body's WRITTEN blocks in text order (an unrelated `if { }` before it moves nothing);
   an impl's is its ordinal among the impls of its block (the lowering's `key`); members under it
   as at module level. Nothing depends on which body was read first.
5. Member bodies are bodies (their module is the block's); a body inside one that holds impls
   again is scanned the same way: a work list, bounded by a depth cap.

**Built (2026-11-16, one runner; nothing in blaze-ladder/src changed).**
- crates/blaze-file-items/src/nested.rs: `scan(text, alloc) -> Nested { blocks: [Block { parent,
  open, close }], items: [NestedItem { block, kind, range }], calls: [NestedCall { block, path }],
  end }`; "5 passed" (sol's shape, serde's, every keyword form, a word in a string / comment /
  macro arguments, an unclosed body).
- crates/blaze-scope/src/blocks.rs `module_text`: the written blocks as `mod __blaze_block_N {
  use super::*; <the block's item statements, verbatim, in text order> <inner written blocks> }`;
  "3 passed". THE RECORD'S FORM today is that text, held like an expansion's
  (`CrateNames::expansions[(body, "blocks")]`, text and outline), and what the lowering makes of
  it in the crate's tables (`impls`, `texts`, `members`, the made modules): no new record type.
- blaze-scope task/lower.rs: `Lowered::bodies: [BodySrc { key, module, text, loc, depth }]`, one
  per fn/const/static item or member whose outline flag is `body_holds_items` and whose text
  holds the word `impl` (so a body without one costs one substring search). resolve.rs
  `make_blocks` (a work list) runs before the fill on the files' bodies, and after each
  expansion and made module on theirs (`sol!`'s `const _` is an expansion's item); the fragment's
  own `mod` declaration is dropped, so no name reaches a block from outside.
- Proof: tests/front2_nested.rs "NESTED impls in the index: 11" and "1 passed, 0 failed" (eight
  bodies that failed with NoMethod / NoAssocItem / Unsatisfied before, the fns of the nested
  impls as bodies of their own, a refused control); front2_order.rs
  `an_impl_in_a_block_does_not_depend_on_order` ("3 passed, 0 failed").

**Not built, each owed.**
- A body that NAMES a struct declared in its own block still gets front2's typed gap ("a struct in
  a block": front2/locals.rs, world_paths.rs `path`): the block's module now holds that struct when
  the block also holds an impl; `Locals::find` should answer struct / enum / alias / trait rows
  from the block's module (and fn / const / static too, so a local item has ONE key: today a local
  fn of a written block has the names library's key and `Locals`' derived key).
- `<X as Tr>::K` and `Self::K` (an associated const as a value) fail for a module-level impl
  exactly as for a nested one (front2_nested `in_const_k` against `module_k`): the type checker's
  `assoc_value`, not this.
- A statement-position macro call that writes an impl: listed by the scan (`Nested::calls`),
  not expanded, no gap raised: the impl stays unseen (a wrong NO for that shape).
- `super::` inside a nested item names the item's module (one level short); a private inherent fn
  of a block impl is private to the block's module (the language: to the enclosing module).
- The scan is not yet a record of its own keyed by the body's text: it runs inside the crate's
  names resolution (D62's "impl index as records" would key `module_text` by the body's digest).
- A const generic argument in braces in a nested item's header before its body
  (`impl Tr<{ N }> for X {}`) ends the item early in the scan.

## Types inside blocks (2026-11-16; WRITTEN, NEVER COMPILED)

The first owed line above, for the tempo census's "1190 gap Local: a struct in a block" (serde's
`Deserialize` derive: `enum __Field`, `struct __FieldVisitor;`, `struct __Visitor<'de>` in
`fn deserialize`). A struct, enum, union or type alias of a block is an item of the block's
MODULE, with one key (the library's) and a head made by the ordinary code:
- blaze-file-items nested.rs `NestedKind::makes_module` (impl, struct, enum, union, alias);
  blaze-scope lower.rs `BodySrc::of` (the word gate is those five words), resolve.rs
  `make_blocks` and blocks.rs `module_text` write a block that holds any of them; blocks.rs
  `block_modules(krate, body, nested)` derives each written block's module key from the same scan.
- front2 locals.rs `Locals::keep` scans the body's text, finds the library's body key (the item,
  or the container's member, whose text is the same slice), and gives each ADT/alias row its
  block's module (`Row::module`, checked with `names.lookup`); world_paths.rs `path` resolves a
  path whose first segment is such a row AT that module with the one name solver; body.rs
  `BodyTypes::env_at` reads a written TYPE at the innermost such block's module.
- Still `Gap::Local`, named: a trait or `mod` in a block; a struct, enum, union or alias that a
  macro call in the body wrote, or that is in the body of an item itself local to a body
  (`LocalKind::unplaced`). Owed: a type written with `super` in a body with block types reads at
  the body's module only; a block's `use` is not read for a written type unless the block also
  declares a type; a local fn of a written block still has two keys.

## Impls inside blocks, runner 2 (2026-11-16)

A WRITE-ONLY leg: every line below marked NEVER COMPILED was written and not run by its writer.

**What the real project said** (tempo-front-6b.txt, the census right after runner 1):
tempo_contracts "heads 48693, built 29198 (60.0%); bodies 20000, clean 2394 (12.0%)" (before
nested impls: 6263 bodies, 3056 clean). Three new classes: "9781 container head: Resolve Path"
(`<impl _serde :: Serialize for Burn>::serialize`), "7443 gap UnreadImpl" (`impl < 'de >
_serde :: Deserialize < 'de > for __Field`: `PathError { .., at: 0, why: NotFound }`), "156
lower Convert: Form" (`<impl alloy_sol_types :: private :: SolTypeValue < Self > for
Call>::stv_abi_encoded_size`).

**One cause for all three** (read from the code, the census lines 42-336 and the derives'
sources; not yet confirmed by a run): crates/blaze-file-items/src/nested.rs `scan` lost the
statement's start at any group INSIDE an attribute. `#[doc(hidden)]`'s `(` was pushed as
`Open::Other { stmt: false }`, which set `fresh = false`, so the item after the attribute was
no item. What the derives write (quoted):
- serde_derive 1.0.229 src/dummy.rs `wrap_in_const`: `None => quote! {
  #[allow(unused_extern_crates, clippy::useless_attribute)] extern crate serde as _serde; }`
  (and `Some(path) => quote! { use #path as _serde; }` under `#[serde(crate = "..")]`), inside
  `const _: () = { #use_serde _serde::__require_serde_not_serde_core!(); #code };`. The
  `extern crate` was dropped from the block's module: `_serde` named nothing: classes 1 and 2
  (the 7,443 bodies are every body offered one of some thirty unread `_serde` impls, the
  largest `impl<'de> _serde::Deserialize<'de> for __Field` of reth_execution_types at 4,567).
  tempo's `sol!` calls pass `extra_derives(serde::Serialize, serde::Deserialize)`
  (crates/contracts/src/lib.rs:32); alloy-sol-macro-expander 1.7.3 writes no
  `#[serde(crate = ..)]` (its only serde attribute is `#[serde(other)]`, expand/enum.rs:50), so
  tempo takes the `extern crate` arm.
- alloy-sol-macro-expander 1.7.3 src/expand/mod.rs:931: `#[doc(hidden)] #[allow(dead_code)]
  type UnderlyingSolTuple<'a> = #sol_tuple;` in the `const _` block, named by
  `stv_abi_encoded_size` (expand/struct.rs:98-99): the alias was dropped: class 3.
- serde's `fn deserialize` body: `#[allow(non_camel_case_types)] #[doc(hidden)] enum __Field`,
  `#[doc(hidden)] struct __FieldVisitor;`: dropped the same way.
The five suspects of the brief, (b) to (e), are not the cause: the path fails at segment 0.

**Written.**
- nested.rs `scan`: a group opened inside an attribute's brackets is the attribute's
  (`in_attr`: no block, no call, the statement stays at its start). `Skip::closed`: an item
  with a body ends at the `}` of its first top-level `{` only when the next token is not `>` or
  `,` (a braced const argument, `Tr<{ N }>`; no statement starts with either). `NestedCall`
  gained `range` (the whole call). Tests: `an_attribute_with_arguments_keeps_its_item`,
  `every_attribute_shape_keeps_its_item`, `a_braced_const_argument_does_not_end_an_item`,
  `a_statement_call_has_its_whole_range`.
- blaze-scope blocks.rs `module_text`: a statement call of a WRITTEN block is written into the
  block's module as the item-position call it also is (`;` added after `(..)` and `[..]`), so
  the one expansion driver expands it and an impl it writes lands in the tables, read in the
  block's module. resolve.rs `expand_decls`: an expansion in a block's module that does not
  read as items and holds no item or statement call (an expression: `assert!(..)`) writes
  nothing and raises nothing; one that mixes items with other statements is refused by name
  (`ExpandNote::NotItems`).
- blaze-scope src/tests/blocks.rs (two crates, a stand-in for serde): the derive's exact shape
  for Serialize and Deserialize, the `use path as _serde` arm, a statement call by path, a
  block's own `macro_rules!`. blaze-ladder tests/front2_nested.rs: `attr_extern_crate`,
  `attr_alias`.

**Class 2's filing.** front2/buckets.rs already files an unread impl whose text names a trait
as a trait impl (`written_trait`, `file_by_text`): never a candidate of an inherent question;
"filed without its trait or self type" is the gap's fixed text (world_items.rs:105). What made
one impl reach 4,567 bodies is `unread_any`: an unread trait impl whose self type has no shape
is a candidate of every trait question its crate's `Home` allows. Not changed here (the file is
the ITEMS lane's); with `_serde` bound the class should be gone.

**Run by the front-end build integrator (12:34 to 12:49, tree 2805c7ecac, load 21.5).** One slip
fixed by it (tests/blocks.rs `println!` -> `std::println!`). "test blaze-file-items: exit 0; 9
passed, 0 failed"; blaze-scope blocks filter "8 passed, 0 failed", whole suite "172 passed, 1
failed" (the known `a_deep_cold_fill_equals_the_plain_reference`); front2_nested "1 passed, 0
failed"; front2_order "3 passed". Census tempo-front-7.txt: "TEMPO tempo_contracts (crate 853,
edition E2024): heads 36707, built 36707 (100.0%); bodies 20000, clean 12749 (63.7%); 24.3s"
(6b: built 29198 of 48693, clean 2394; before nested impls 3056). "container head: Resolve
Path", the `_serde` UnreadImpl class and the SolTypeValue Convert class are gone; no SolCall /
SolEvent / SolError class; 0 "does not read as items"; names once 23.7s (6b 46.3s). The fix was
not run with its cause removed (the planted-bug run of rule 8 is owed). The printed lines
"NESTED impls in the index" and "BLOCK impls by textual scope" were not seen (lanecheck prints
summaries only).

**The census's next classes (tempo-front-7.txt), none this leg's code.** "2851 gap UnreadImpl:
at" (impl e97737e3.. at /package/907/src/core.rs:458, offered to bodies of tempo_contracts:
`unread_any` again); "1190 gap Local: a struct in a block" (`<impl<'de> _serde::Deserialize<'de>
for Role>::deserialize`: serde's `struct __Visitor` named in its own body: item 3 below, now the
largest class this design owns); "849 RefusedImpl [PartialEq]" (the refused alias
`FieldElement`, /package/317/src/field.rs:86); "831 RefusedImpl [Error]" and "704 RefusedImpl
[Debug]" (Resolve/Path at /package/650/src/errno.rs:176 and /package/965/src/io/errno.rs:41).

**Owed, in order.**
1. A statement call in a body with NO written impl is not expanded (the body makes no module;
   `BodySrc::of` also asks for the word `impl` in the text): an impl it writes is unseen and
   no gap is raised: still a wrong NO. A gap "for every trait and every local type" on each
   such call would fire for every `println!` statement; the sound fix is to expand statement
   calls of every body (a cost to measure first), or to ask the macro's definition whether
   any arm's text holds `impl`.
2. A `macro_rules!` written before the fn in its MODULE is not in a block module's textual
   scope: task/answer.rs `entry` reads `scope.mark(module)`, and a block's module has no `mod`
   line in its parent (tests/blocks.rs `blocks_expand_a_statement_call_named_by_textual_scope`
   prints "BLOCK impls by textual scope: N of 3"). Paths (`serde::m!`, `crate::m!`) and the
   block's own `macro_rules!` are found.
3. `super::` inside a nested item one level short; a private inherent fn of a block impl is
   private to the block's module; `Locals::find` answering local types from the block's
   module (runner 1's list, unchanged).

## Speed on real code (2026-11-16)

Stage one of D62, the checker's side. Measured with `crates/blaze-ladder/examples/check_speed.rs`
on bstd, release, `--threads 1`, outputs in port27/speed-1116/ (bstd-2.txt before, bstd-3.txt
after).

**Counters.** asks.rs `Tally` is the ask log's summary form: the one seam (`Reads::record`)
counts rounds (checks), questions by kind, missing answers, table conversions with entries
converted, reached and offered, and splits a check's time into the world's answers, hashing
them into the read list, and the rest; the caller passes the clock (`check_counted`;
front2's `check_body_counted`). Before (bstd-2, load 18.68 at start): pass A "2300 bodies in
90004.1 ms: 26 bodies/s", B "61 bodies/s"; A "rounds: 6294 checks for 2300 bodies; bodies by
rounds 1:1298 2:353 3:163 4:140 5:93 6+:253"; A "questions: 1412447 (4054 missing), 224.4 per
check"; A "table conversions 208705: entries converted 798380018 .. (3825 per conversion)";
A "time in checks 88457.9 ms: world answers 1028.6 ms, hashing answers 57008.3 ms, the rest
(walk, solver, conversions) 30421.1 ms". The world's answers cost 1%; hashing every answer's
whole table into the read list cost 64%, converting it most of the rest.

**What changed.** (1) `FactCache` converts from an answer's roots by a work list, each node
once per check, into one table per check whose handles are absolute (an answer's portable
is that table's prefix); a memo from (table, node) to the solver term lives in the
`FactCache`, which lives for one check (`InferCtx`), so nothing outlives the call. A child
after its parent is `Malformed::TypeOrder { node, child }`. (2) A record with a table
(`world::Tabled`: trait, impl header, predicates, ADT, signature, opaque, const params) is
hashed into the read list as the record without its table plus a Merkle digest of each term
its handles reach (`Hashes`, memoized per run); the solver facts' digests reuse that entry
(`Reads::digest`). (3) One walk order of a node's children (portable.rs `node_children`) for
both. A table's identity in both memos is its address, valid because every answer is borrowed
for the whole check; it is never part of a key or an output.

After (bstd-3, tree 0c1585c054, load 29.07 before and 20.97 after): A "2300 bodies in
2468.5 ms: 932 bodies/s (floor 71747/s: ratio 77.0x)", B "2871 bodies/s (.. ratio 25.0x)"; the
same outcomes ("clean 2204; check refused .. 96"; "outcomes changed A->B: 0"); A "entries
converted 114626; entries reached 348467; entries offered 798380018" (converted: 6,965 times
fewer); A "time in checks 1883.1 ms: world answers 278.0 ms, hashing answers 140.6 ms, the
rest .. 1464.5 ms". The edit loop: smallest clean body over the crate's table 4687.6 us before,
51.4 us after (its own records' floor: 49.6 us, ratio 1.04); median 11259.2 us before, 151.2 us
after (floor 136.5 us, ratio 1.11). front2_order (2 passed), front2_checks (1 passed) and
blaze-typeck's suite (114 passed) held after each change.

**What remains, by cost, and whose.** From bstd-3:
1. Rounds (this lane's next step, not built): "first-pass extra (records made, extra rounds)
   1667.5 ms (67.6%)" of pass A; "rounds: 6294 checks for 2300 bodies; bodies by rounds
   1:1298 2:353 3:163 4:140 5:93 6+:253". A round stops at its first missing record
   (`Reads::finish_need` returns that one plus the run's reads as hints), so 649 bodies take
   three or more. Next: go on past a missing record with a hole where the walk can, and
   return every need at once, so a cold body takes two rounds.
2. The impl index per command (the front end's: buckets.rs): "Buckets::new alone .. 190.1
   ms"; the smallest clean body cold is "check 192709.7 us" and "1484.5 us" with the buckets
   prebuilt (130x).
3. Member signatures built with their impl (the ITEMS heads lane): the median clean body cold
   spends "container 30851.6 us" and makes "heads made 447 .. fns 345".
4. The store bound to one thread (`Names` holds a thread-bound `Alloc`): "sync: Names false
   ..; Store false"; on one thread "setup (toolchain+names+front2) 4331.7 ms" against "check
   2688.1 ms" for the whole crate; every worker would redo it.
5. Inside the checker, warm: B "the rest (walk, solver, conversions) 662.8 ms" of 796.6 ms,
   203.3 questions per check, ratio 25.0x to the no-world floor: the next profile is the
   solver's, not the world's (world answers 85.9 ms, hashing 47.9 ms).

**Holes: every need in one round (built 2026-11-16, second runner).** A missing world answer
is a HOLE (world.rs `Hole`), a different thing from a GAP: the seam (`Reads::record_with`)
records the question and hands the rule the answer type's stand-in (`Holed`: `None`, `&[]`,
empty predicates), and the walk goes on. A run that met any hole makes NO result (lib.rs
`run`): not its types, not its diagnostics, not an error met past the hole; it returns
`Need { missing: every hole sorted, first: the one firm need, questions: the hints }`. So a
hole can change no answer, and a rule needs no case for it. Holes met after the first are
speculative; the caller (front2 check.rs `make_round`) drops a fill it cannot make while
another fill of the round was made (counted: "speculative needs dropped N"); a round that
made nothing names the least refusal, as before. A `Stop::Need` a rule or the solver makes
itself still ends the walk (it joins the holes): a fact fill that reads a hole's `None`
answers `Malformed`, so the solver's chains stop the walk early: what is left of rounds.
Tests (blaze-typeck tests.rs): `five_missing_records_come_back_in_one_round` failed before
the change with "the first round named 1 of the 5 missing records: [FnSig(ItemKey(1)),
Predicates(ItemKey(1)), Adt(ItemKey(327680)), Lang(Deref)]" and passes after (two rounds,
clean); the rounds oracle (a record after N rounds equals one run over the full world, byte
for byte) with two planted bugs, each caught: `HolePlant::KeepEarlierReads` (an earlier
round's questions enter the read list) and `HolePlant::AnswerThroughHoles` (a holed run makes a
result; `a_hole_never_makes_a_diagnostic` shows the control's stand-in diagnostics). The ask
log's `Order::Pull` model (stop at the first miss) is now an upper bound on rounds, and its
control loses as many rounds' fetches as the bound allows.
Counters added for the solver's profile (asks.rs `Solve`, printed by check_speed as "solver:"
and "checker phases:"), and `late_holes`: the holes second and later runs meet, by kind.

Measured (port27/speed-1116/bstd-4.txt, tree 6db6fbf05e, load 16.9 before, 14.7 after; the
tree moved under other lanes since bstd-3: 2308 bodies, not 2300):

| | bstd-3 (stop at first miss) | bstd-4 (holes) |
|---|---|---|
| A rounds | 6294 checks; 1:1298 2:353 3:163 4:140 5:93 6+:253 | 3917 checks; 1:1292 2:646 3:259 4:69 5:19 6+:23 |
| A check | 932 bodies/s (ratio 77.0x to 71,747/s) | 912 bodies/s (78.6x) |
| B check (warm) | 2871 bodies/s (25.0x) | 2578 bodies/s (27.8x) |
| first-pass extra | 1667.5 ms (67.6%) | 1634.5 ms (64.6%) |

Checks fell 38% and bodies of six rounds or more from 253 to 23, but pass A did not get
faster: its extra cost is the records made, not the repeated checks. Outcomes: "A outcomes:
clean 2223; check refused .. 85; lowering refused 1" equals B's line, and B never meets a hole
(every record present: "B rounds: 2308 checks for 2308 bodies"), so on the same tree the holed
first pass decides exactly what a hole-free run decides ("outcomes changed A->B: 0"); the
counts differ from bstd-3's "clean 2204 .. 96" because the lowering moved ("lowering refused"
9 to 1), not because of holes. Speculative needs dropped: 7.

Why bodies still take three rounds: "holes met by second and later runs: ImplsOf 1400; Inherent
84; Adt 79; TraitDef 43; AssocItem 39; Path 19": a trait goal's impls name, through their
where-clauses, the next trait whose impls are needed (a record naming a record); and a fact
fill that reads a hole's `None` ends the walk (`Malformed` in facts.rs `fill_impl`/
`fill_trait`/`fill_adt`/`fill_sig`): the first is a chain, the second is left to fix (make the
fill answer the solver "ambiguous" on a hole: infer/mod.rs's three solver loops, the checker's).

The solver's profile, warm (B): "solver: 107678 calls in 514.9 ms (4.8 us each), 79210
repeated after a fact fill; frames 170616 (1.6 per call); cache hits 307836; impl candidates
933736 (5.47 per frame)"; "walk and rules .. 144.6 ms"; world answers 106.2 ms, hashing 52.5
ms, fact fills 71.3 ms, of 889.5 ms. Top three: (1) the solver, 514.9 ms (58%), and 74% of its
calls are a goal started again from the top after one fact was filled: inside each check the
solver repeats the round pattern this section removed from the driver; (2) world answers and
hashing, 158.7 ms; (3) the walk, 144.6 ms. Not fixed here: the cause of (1) is plain (re-entry
per fact), and its fix belongs to blaze-solve (the checker lane): a solver that asks all the
facts a goal's candidates need at once, or facts.rs filling an impl set's headers with the set.

**What remains, by cost, and whose (after bstd-4).**
1. Records made in the first pass, 1634.5 ms of A's 2529.8 (the front end's: heads and
   buckets, items and types lanes).
2. The solver re-entering a goal per fact, 79210 of 107678 calls, 514.9 ms warm (blaze-solve,
   the checker lane; or facts.rs prefill, this lane).
3. Rounds from fact fills that meet a hole and end the walk (infer/mod.rs, the checker lane),
   and from impl where-clause chains (ImplsOf 1400 late holes).
4. Owed: a test that a hole met inside a method probe (`instantiate_method` passes over a
   candidate whose header is `None`) gives `Outcome::Need` and no diagnostic (the checker lane
   asked for it; note (12)).

## Not decided here

- Whether a run stopped at `Need` should save its state instead of re-entering: decided by a
  profile of re-entries on reth and tempo.
- Regions: typeck erases them as rustc does; the borrow checker keeps its own.

## Path patterns (2026-11-16)

- Built (blaze-lower): every identifier pattern is settled once, at the end of the lowering
  (machine.rs `settle_patterns`), innermost first: an item declared in the body's blocks
  (`local_value`: const, static, fn, unit/tuple struct by its text), else the caller's oracle
  `ValueNames::value_name` (lib.rs; one question, the module's value-namespace answer as a
  `ValueName`). A bare const / unit struct / unit variant becomes `Pat::Path`, and every
  `Expr::Local` that named its binding becomes `Expr::Path`; static, tuple ctor, or a
  `ref`/`mut`/`@` binding of any of those is `DiagKind::PatternShadows` (E0530) at the name.
  `Pat::Bind`'s `candidate` field is gone. Every oracle answer is in the record's read list
  (`value_reads` column, `View::value_reads`, `value_reads_hold`); key domains bumped.
- `None =>` had no special case: it bound a variable named `None` (the arm always matched).
- front2 answers through `Resolver::path` and the store's ADT heads (body.rs `ModuleValues`).
  OWED: front.rs (the old front) and the census/stats examples pass the `NoModule` fixture;
  a local `use E::*;` inside a body is not seen (local `use` items carry no name: the
  items-inside-bodies lane's resolution); const generic parameters as patterns (E0158).
- Tests: blaze-lower tests.rs `identifier_patterns_are_paths_where_the_name_is_one`,
  `a_binding_that_becomes_a_constant_is_lowered_again` (115 passed). front2_clone.rs
  `unit_struct_patterns_through_front2` (11 cases) NOT RUN: blaze-consteval red under the code
  builder's `blaze_build::ground` batch; still `#[ignore]`d.

## Fixes ranked by what is left (2026-11-16)

The owner, on `fn add(a: u32, b: u32) -> u32 { a + c }`: the help offered `a` (one letter from
`c`); `b` is right, the parameter nobody used. The rule: a fix is ranked by what is left wrong
after it.

Design (built):
- Where it lives: a new side table `Checked::suggests` (blaze-typeck rules/suggest.rs
  `Suggests`): per E0425 at a one-segment value name, its item-relative range, the expected type
  there, and the bindings in scope whose type fits it, each with "used nowhere" and its scope
  depth. No cross-crate change: `Checked` is built by literal only in writeback.rs, `Diag` and
  `DiagCode` are untouched, no `Question` and no `World` method was added, so no batch token.
- The walk notes a hole only when `expr_path` pushed an E0425 at a one-segment path
  (`Machine::named_holes`, with the frame's expectation); writeback keeps the holes whose E0425
  survived (a rolled-back probe's do not), exports the expectation (a variable, or an export that
  would add an E0282, is "no expectation"), and `suggest::collect` lists candidates.
- Rules: (1) type: a candidate whose type is not the expectation is not recorded (the
  expectation, else for an operand of a builtin arithmetic/bitwise/comparison operator the other
  operand's scalar type); (2) used nowhere ranks first, a `_name` never counts as unused;
  (3) spelling: optimal-string-alignment distance, at most `max(len, 3) / 3` for a candidate (2)
  does not rank; (4) scope depth. Two best equal on every count: no fix, both named.
- The choice needs spelling, which is text, and the checker holds none (a `Name` is a hash):
  `suggest::pick(&Suggests, &Hole, &dyn NameText, Rules, alloc)` is a pure function of the
  record and the item's text (already an input of the body's key: every spelling it reads is a
  binding's `Name`). It returns `Pick::{Nothing, Unused, Spelled, Tied}`; `Pick::fix` is data:
  `Fix { at, from }`, the hole's item-relative range replaced by the item's own bytes at `from`
  (the binding's name, trimmed of `ref`/`mut` in `pick`). No string is stored.
- Words: blaze-report frame/words.rs `HELPS` (`HelpKind::{NeverUsed, SpelledAlike, Tied}`);
  drawing: frame/mod.rs `help` (`help: ..`, then `N - line` and `N + line`).
- Cost: zero on a clean body (`steps == 0`, no allocation: one length compare per path at the
  walk); with a hole, one pass over expressions (parent column, use marks), then per hole its
  ancestors and the patterns of the scopes passed, shadowing in a hash set: 200 locals, 404
  steps (the floor: each expression and pattern once). A list for shadowing was 20304.
- Tests: blaze-typeck suite/suggest.rs (7 pass: owner's case and its control without rule (2):
  "tied a b", the type filter, spelling with transposition, a tie, nothing in scope, the hole is
  no use, `_b`, cost); blaze-report frame/tests.rs
  `the_owners_case_says_why_b_and_draws_the_line_before_and_after` (exact bytes).

Owed:
- Module items (`halp()` with `fn help()` in the module): needs a world question listing a
  scope's value names WITH their text (a new `Question` variant breaks blaze-real check.rs's
  exhaustive match and `World` gains a method: one batch). Candidates then carry
  `Taken::Item(Def)` and rank after locals at equal counts; an item is never "unused".
- The glue: the driver calls `pick` with the item's text, prints the candidate's type for the
  words, adds the item's place to the ranges, and calls `frame::help` after the finding; `blaze
  check --fix` applies `Fix`.
- Extensions, same ranking: a field that does not exist (E0609/E0560): candidates are the ADT's
  fields; (1) the field's type against the expectation; (2) in a struct expression, a field not
  yet initialised, at a read, a field never read in the body; (3) spelling; tie: none. A method:
  candidates by the receiver's inherent and in-scope trait methods; (1) arity and the expected
  return type; (2) none (methods are not "unused" locally); (3) spelling. A type name: the types in
  scope; (1) the kind the position needs (a trait in a bound, a type elsewhere), generic arity;
  (3) spelling. A path of several segments: rank the first unresolved segment among the names of
  its parent module or type, with the same counts.

## Generic associated types (2026-11-16; WRITTEN, NEVER COMPILED)

The form: a projection's list is the trait's arguments, `Self` first, then the associated type's
own type and const arguments in declaration order (`<T as Tr<A>>::Out<B>` is `Alias { args: [T, A,
B] }`); an impl's `type Out<X> = ..;` is a type over the impl's generics then `X`, which the solver
instantiates with the impl's arguments and the goal's own (blaze-solve alias.rs `step_impl`).
Lifetimes are in no list: a portable type holds no region, so `type Iter<'a>` has no own parameter
and `Iter<'a>` no own argument, as every other lifetime argument is treated.

- The checker is told the counts by a new question, `Question::AssocGenerics(trait, name)` ->
  `AssocGenerics { parent, own }` (world.rs; `World::assoc_generics` is defaulted to `None` for
  worlds that declare no generic associated type: make it required in the next batch over the
  worlds). infer/facts.rs files `own` in `AssocTy`, `ImplAssoc` and `AssocDef` (`own_generics`,
  `fill_trait`, `fill_impl`, `fill_assoc`). rules/assoc_args.rs `alias_arity`: a written projection
  with the wrong count is E0107 (`Because::ArgCount`, the own counts) and the error type
  (machine.rs `lower_node`, `Lower::Body` only). Tests: suite/gat.rs.
- The front end: world_items.rs answers the question from `Maps::assoc_generics`; heads.rs
  `Store::impl_` reads an impl's `type Out<X> = ..;` with `X` in scope after the impl's parameters;
  gat.rs `Builder::gat_bindings` makes `U: Tr<Out<T> = X>` the projection clause with `[U, T]`
  (called by preds.rs `one_bound`; const_args_in_bounds.rs `angle_args` no longer refuses the form
  for that caller). Tests: tests/front2_bounds.rs `gat_bind`, `gat_impl`, `gat_where`.
- Holes: machine.rs `lower_at` no longer memoizes a node that holds a `_`; `lower_holes` lowers it
  once per written occurrence (`(_, _)` is two variables). Test: suite/holes.rs.

Refused by name (front2 `Why::Unsupported`): "a const argument in a binding of a generic associated
type (`Name<3> = U`)"; "a binding with type arguments of an associated type whose own parameters
are not on record"; "a where clause with a trait bound on an associated type (`type Out<T> where T:
Bound`)"; "a `where const` condition on an associated type"; "a `use<..>` bound in an associated
type's where clause"; "more than 8 outlives where clauses on one associated type"; "an associated
type binding on a trait alias (`Alias<Name = T>`)"; and, where the trait is not known to the caller
(a trait object, `impl Trait`), still "a binding of a generic associated type with type or const
arguments (`Name<T> = U`)" and "an associated type bound with type or const arguments (`Item<T>:
Bound`)".

Named gaps, not refusals: gat.rs `OUTLIVES_NOT_CHECKED`: an outlives clause on an associated type
(`where Self: 'a`) is read and kept (`Maps::assoc_where` for a trait's; an impl's is read and
dropped after the same refusals) and nothing checks it. A bound on an associated type's own
parameter (`type Out<T: Clone>`) is not in scope for a `T::X` shorthand inside the impl's value.
The solver's object candidate still skips a generic associated type's bounds (builtin.rs).

## Constants in types: three states (2026-11-16; WRITTEN, NEVER COMPILED)

A constant written in a type (an array length, a const argument, an array repeat length) is in one
of three states where the front end builds a signature or converts a body (front2/const_open.rs):

1. A value (`PNode::ConstValue`).
2. One const parameter in scope, alone (`PNode::ConstParam`).
3. Open: an expression over a generic parameter, or over a `Self` that is not ground.

The Reference, "Generic parameters", "Const generics": "const parameters may only appear as a
standalone argument inside of a type or array repeat expression. [..] they cannot be combined with
other expressions", with `fn bad_function<const N: usize>() -> [u8; {N + 1}]` as its example of
what is not allowed. So state three is in no accepted program without the unstable
`generic_const_exprs`, which the pinned library does not turn on. It stays a typed refusal naming
the constant by its range ("a constant expression over a generic parameter": a head's refusal, a
body's `ConstGapWhy::OverGeneric` gap). The four measured classes were all state one, refused for
other reasons:

- `[u8; size_of::<Self>()]` in `impl u16` (core's `from_le_bytes`; the 255 `check gap: FnHead`):
  `Self` was taken for a parameter. `const_open::ground_self` tells a ground `Self` (a built-in
  type, an ADT with no generic arguments) apart; `anon::Ask::of_self` files the anonymous constant
  with that `Self` in scope, its key over (module, type, text, what `Self` is).
- `size_of::<usize>()` in an impl header and in a repeat length (21 and 23): `size_of::<T>()` is
  `<T as SizedTypeProperties>::SIZE`, and the evaluator read a constant path at no arguments
  (`ConstFail::TraitConstNoSelf`). blaze-consteval machine/code.rs `qualified_const_args`: a path
  `<Q as Trait<A..>>::NAME` gives `[Q, A..]` grounded at the instance, then `ConstWorld::resolve`.
  The front end's world makes the trait's candidate impls when its selection finds none and asks
  again (`ConstsWorld::wanted`, `const_open::impl_rows`); a run that stops on `Refusal::Missing`
  marks its code not kept (machine.rs `Op::Late`), so the second asking compiles it again.
- `const MOST: usize = ..; [x; MOST]` (44 `lower: Convert`): `const_open::local_const` finds a
  `const` declared in the body before the module is asked (a block's item shadows the module's);
  its head is made before its own body is checked (`const_open::local_head`).

A consequence, and its fix: a method of `u8` now has a signature that needs a constant, and the
check of `u8::MAX`'s body (inside an evaluation) fills `u8`'s inherent impls, so the head asked
for a value while the evaluator was busy (`ConstFail::Nested`). `Consts::value_at` is a work list:
a failure that names a constant to have first (`const_open::needed_first`) has that constant
evaluated and filed (`Consts::first`), then the failed one evaluated again; at most 64, and one
that needs itself returns its refusal. No evaluation runs inside another.

Still gaps, by name: a constant path that is not `<Q as Trait>::NAME` with one segment after the
anchor (`T::NAME`, `Self::NAME`, `<Q>::NAME`) in a generic body is `ConstFail::TraitConstNoSelf`
(the checker records `Def::Const(key)` with no arguments: a column `(expr, item, args)` in
`Checked` is the fix, written where rules/lit_path.rs `assoc_item_ty` has `parent_args`); a
`Self` with generic arguments; a body's `const` named in a const ARGUMENT (`foo::<LEN>()`: the
lowering gives a const argument no block scope to look it up from) or through a local `use`.

Carrying state three (when `generic_const_exprs` is wanted): blaze-ty and the solver already hold
it (`ConstKind::Unevaluated(def, args)`, `Alias(_, Const, ..)`); the portable form does not. One
cross-crate batch: `PNode::ConstExpr { item, args }` (the anonymous constant's key and the
enclosing item's generic arguments; equal when both are), lowered to `const_unevaluated`, written
back, and normalized by a world question for the value once `args` is ground. The front end's half
exists: a filed head per anonymous constant, and `Consts::value_at` evaluates a root at arguments.
Tests: crates/blaze-ladder/tests/front2_const_open.rs.

Hand-on, 2026-11-16 (tempo census 13, brief 11, `TraitConstNoSelf`; nothing of this paragraph is
built). The brief places the cause in consts.rs's settle loop; the need is made in blaze-consteval
machine/code.rs, `raw`, arm `Expr::Path` / `Def::Const`, and consts.rs only refuses it. A
QUALIFIED path (`<u8 as Tr>::N`, the brief's own test) already gets its arguments there from the
path (`qualified_const_args`); test `a_constant_whose_body_reads_a_trait_constant_has_its_value`
is written for it. What remains is a path the checker selects by type (`Self::N`, `T::N`,
`<Q>::N`): its arguments are in `Checked::const_args` (`const_args_of(expr)`, a list of the
record's `types`), which the evaluator does not read: `blaze_consteval::Typed` has no such
column, and it is built by a struct literal in blaze-std src/eval.rs as well as by `Typed::new`,
so adding the column is a two-crate batch. The end state is one home for a constant path's
arguments: the arm reads `const_args_of(e)`, grounds each at the instance (`cg`), asks `resolve`,
and `qualified_const_args` is deleted. front2/const_at.rs (D84) pulls the same rows for an
instance's own body (`needs_of` over `Checked::const_args`); the evaluator's arm is the same
question inside a constant's body and should take its rows the same way, not a second derivation.

State at 14:35 (generic associated types): the checker's side is green (blaze-typeck "172 passed, 0
failed", suite::gat 2, suite::holes 1). The front end compiles; `front2_bounds` at 14:26 had
`gat_where` clean and `gat_bind`, `gat_impl` stopped by "the bounds of an associated type with type
or const parameters of its own" (preds.rs `assoc_ty_bounds`). That refusal is now built (heads.rs
`Store::trait_members`: the alias lists the trait's parameters then the type's own; the bounds are
read with them in scope): NEVER COMPILED, the rerun is queued in BUILD-STATUS-FRONT.md "Hand-on
14:30". Still refused: "a bound on an associated type's own parameter (`type Out<T: Bound>`)". Gap:
the implied `Sized` of a generic associated type's own parameters is not filed. The tempo census for
the three classes has not been run.

## Head making waits (D161 loops 2 and 4; 2026-11-20, spec; first step WRITTEN, NEVER COMPILED)

Today a head's making restarts from the top in two places. In front2 heads.rs
`Store::ensure_seeded`, each `Step::Need(k)` drops the step's work. The need is made, and the same
want is stepped again from the top. Before 2026-11-20 that meant even lowering its text again. In
check.rs `make`, each `Why::NeedConst(c)` drops the whole fill: `c` is evaluated, and `make_once`
runs again, up to `MOST_CONSTS` (64). The end state is one wait: a head that meets a need parks,
and when the need is made it goes on from where it stopped.

**The frame.** A parked want is a `HeadFrame` (heads.rs) holding:

- the `Want`;
- `FrameLowering`: the item's detail, text and names module, and the text's `Lowered`, made on
  the frame's first step that reaches the text;
- (step 2) a `HeadCursor`: how far the head's own builder got. For an ADT, it is the generics
  filed and the index of the next field. For a trait, the supertraits, the members done and the
  next member. For an impl, the header done and the next member. For a fn, the inputs done. The
  cursor holds only indices into the lowering and ids of rows already filed in the store. It
  holds no borrowed view, so a frame can move between stack slots;
- (step 3) the pending need: a `LibKey` head, or a constant `c` with its place.

**Where it yields.** Every `Why::Need(k)` and `Why::NeedConst(c, at)` a builder raises, and
`Step::First(w)` (an impl before its member fn). Each is a point where everything before it is
already filed in the store, so resuming means "skip what the cursor says is filed". The members'
extra needs (`more_needs`) are made before the frame resumes, as today.

**What `Maker::step` returns** (blaze-ask, when the store is driven by the asker):

- `Step::Needs(Need::head(k))` at a head need;
- `Step::Needs(Need::constant(c))` at a constant need. The evaluator is another maker on the same
  asker, so `MOST_CONSTS` and `ConstFail::Nested` go away. A constant met inside an evaluation
  parks like any need, and a constant cycle is the asker's `AskError::Cycle`;
- `Step::Made(ordinal)` when the want is filed;
- `Step::Failed(code)` for a refusal (`HeadError::Item`'s `Why`).

The frame is the asker's `Frame` type, and `start` is a frame with nothing lowered.

**Cutting `Store::step` into steps.**

1. Lower once per frame. This is BUILT (2026-11-20, this lane): `ensure_seeded`'s stack is
   `Vec<HeadFrame, Alloc>` on the store's allocator, and `step` takes the frame's
   `&mut Option<FrameLowering>`. A step after a need re-runs the builder, but never the parse.
   The lowering is a function of the text, the crate's cfg and its edition, and no step changes
   any of them. The names reads repeated by a re-run land in the recorder's sorted set, so the
   read list is unchanged.
2. Give each builder (`adt_head`, `adt`, `trait_head`, `trait_members`, `impl_`, `member_fn`,
   `fn_head`, `alias`, `trait_alias`) a cursor argument. Each loop over fields, members or inputs
   starts at the cursor and bumps it after each filed row, so a resume skips the rows before it.
   Done one builder at a time. `Cost`'s `census.made_again` (tables/cost.rs) is the meter: it
   counts steps that stopped and were stepped again, and `stopped_ns` counts their time. Each
   switched builder should drive `stopped_ns` to the cost of the one row that stopped.
3. Turn `NeedConst` into a yield. `ensure_seeded` gains the evaluator as a second maker, which
   parks the frame instead of returning the `HeadError`. check.rs `make`'s loop and `MOST_CONSTS`
   are deleted with it. Then the check.rs rounds that restart on a world miss (`callee_target`,
   `reach_capture_copy`, `MOST_ROUNDS`) become one ask of the missing head, once a resumable
   `ensure` answers it in place.
4. Move the store onto blaze-ask's `Asker`. `ensure_seeded`'s own stack is deleted, and so is the
   `just_made`/`NeedMade` probe: a need that is published and still not found is the asker's
   `AskError::Word`.

**Memory per parked frame.** A `Want` (two keys at most), one `ItemDetail`, two keys, and one
signature's `Lowered`. That is the item's head form: about a hundred bytes of rows per field,
member or input, plus the text's lowering arena. It is held only while the frame is parked: each
frame drops when its want is made. The cursor adds a few `u32`s.

**How deep the stack gets.**

- **The bound.** `ensure_seeded` pushes at most one frame per step and stops at `MOST_STEPS`
  (65,536). Under blaze-ask the bound is the asker's `MOST_DEPTH` (256), and `StackFull` names the
  need.
- **Tempo's deepest head chain: NOT MEASURED.** Nothing was run. A chain is a sequence of heads,
  each needing the next before it can be filed: a supertrait, a bound's trait, an alias's target,
  a field's ADT. Over a seeded store most needs are `Seeded::Filed` and push no frame, so the
  depth is the chain of heads lowered in that one store. To measure it: record `stack.len()`'s
  maximum in `Cost` (beside `census`) and read it on tempo's Bodies stage.
- **Expected.** Tens. Whether 256 is enough is unknown until that measure exists, and it gates
  step 4.

## The gate fetches, the recorder records (D161 (a), D168; 2026-11-20, spec)

The gate is blaze-ask gate.rs's `GateSet`. Its rule: prefetching never changes a read list. The
gate fetches. The recorder records only what the check asks.

- **The set.** On a body's first check, it is the syntactic prefetch: the paths, traits in scope
  and lang paths the text names (blaze-typeck world.rs `prefetch_syntactic`, collected by front2
  asks.rs `Gate::note`). On an edit, it is the body's LAST read list's head cells (33 cells for a
  small tempo body, D168).
- **The fetch.** One entry: front2 tables/seeds.rs `fetch_cells(store, keys, seeds, cells,
  scratch) -> Result<Fetched<'c>, FetchError>`. The gate and the engine's writer both call it. It
  files each cell from its frozen record before the body is entered. `seeds` reads names through
  `Tables::reading_into(&FetchReads)`, a sink that keeps nothing, so no fetched cell reaches the
  recorder. A cell the check asks is recorded by the check's own fact (task.rs `fact_read`), as
  without a fetch.
- **The proof.** `Fetched` can be made only by `fetch_cells`. The body's entry takes it, and takes
  blaze-ask's `Settled` from `GateSet::settle` with `published = Store::made` over the same cells.
  A cell left to the body's store (`Seeded::Lower`: an item of the body) is the one exception, and
  it is counted (`Fetched::left`).
- **The oracle.** One body checked fetched and unfetched gives equal findings, equal record
  bytes and an equal read list. The planted control fetches through the body's own recorder
  instead of `FetchReads`, and the lists differ.

Not wired yet: `record_body_as` taking `Fetched`. Its callers are the engine's (blaze-cli), so the
signature change is one batch with them.

## A read is offered once (the recorder piece)

tempo's profile: `bodies: reads offered 757804987, kept 22834789`. That is 33 offers per kept read,
about 8,800 per body over 85,828 bodies.

**Who offers the same cell again (counts from the code, not measured).** A one-segment plain path at
module `M` goes through blaze-scope names.rs `walk`, `lexical` and `in_source`, under
names_cells.rs `Logged`. It offers these:

- `Module{M}` four times: `chain`, `scope`, `chain` again on a miss, and `no_implicit_prelude`.
- `Root` twice.
- `Module{prelude}` twice or three times.
- `Module{G}` for each glob the walk goes through.
- One `Binding` for each module asked.

That is 10 to 16 offers for one name. Only the bindings are new to the body: the module, root and
prelude cells are the same for every path in it.

The repeat that removes most is the question, not the cell:

- `Reads::prefetch_syntactic` answers every path, lang path and trait-in-scope through the world.
  The checker then asks each one again.
- Every ask reached `BodyWorld` again (`again(|| x_once(..))`), and `x_once` walked the names again.
- The recorder only noticed a repeat after the offer: a compare with `last`, then a probe of a
  hash set.

**1. Record at the first answer.** `blaze_typeck::Reads::heard` is the run's memo: every question
the world answered with `Ok` in this run, the prefetch's answers included. Each ask tells the world
`World::asking(Some(Asking { kind, repeat }))` before it asks, and `asking(None)` once the answer is
in. `BodyWorld` passes this to its names source, the `Tables` view passes it to its `ReadSink`, and
the `Recorder` keeps it. Under `Repeats::Quiet`, a repeat's offers return at the first instruction.
They are counted as `memo`: no hash, no placement, no push.

A hole is not in `heard`, because the next ask may be answered by reads the hole did not make. The
typeck read list is unchanged: its rows, the digests of its answers, and the sort and dedup at the
end. This removes every repeated question's offers. The answers are still computed again. A
question → answer table would also remove that compute, and it is the next step: one typed column
per kind in `Reads`, keyed like `heard`.

**2. The recorder's structure.** A 64-slot direct-mapped filter (`recent`, chosen by `slot_of`: one
byte of the key, folded with a tag for the variant) replaces `last`. A recent repeat costs one load
and one compare (`filtered`). A miss probes `seen` as before. The sort at the end is unchanged, and
it sorts only distinct reads.

**3. Coarse covers fine.** This is counted, not applied (`covered`). blaze-cli names.rs gives
`Section::Names` the bytes of the names record. A `Binding`, `Module` or `Textual` cell of a crate
whose `Whole{Names}` the body already kept is therefore covered by that whole read. Dropping those
cells changes the read list, which is a list change with its own oracle, so it is not part of this
piece. Items cells are not counted: their digests read file text too (names_cells.rs
`members_digest`).

**Floor (rule 67).** 1.0 offer per kept read, when every question is distinct and each answer
offers each of its cells once. On tempo the expected ratio, not measured, is about 4 to 8. It comes
from the walk's own repeats inside one distinct question (the 10 to 16 offers above for 2 or 3 new
bindings), and the filter answers most of those without a probe.

**Counters.** `ReadTally` now carries `memo`, `filtered`, `covered` and `by_kind`. `by_kind` has one
slot per `QUESTION_KINDS` entry, then `outside` (setup, lowering, and the verdict's fact reads).
blaze-cli engine_bodies.rs prints a second `bodies: reads` line with these and with offered-per-kept
against the floor, then the ten question kinds that offer most.

**Oracle (rule 78).** tables/task.rs `compare_repeats` runs one body twice, with a fresh recorder
under `Repeats::Offer` and one under `Repeats::Quiet`. It requires equal verdicts and byte-equal
sorted read lists. It runs on every body that any test checks under `--features asks-oracle`.
tests/front2_alias_opaques.rs runs it on three seeded bodies and asserts:

- `quiet.offered + quiet.memo == offer.offered`;
- that some question repeats;
- the control: `RepeatsPlant::DropFirst` makes the Quiet side drop its first names read, and the
  oracle must see it.

recorder.rs tests the same seam on a tape. Its control is a repeat that reads a cell its first
answer did not read.

**Expansion demands (337,918 for 26,744 keys).** On this side, a `Records` demands each call's
expansion record once:

- the items memo (`ItemsRecord::whole`, `seen`) checks each text once;
- `OriginParts` keeps the origins that came with the same read;
- `Records::text_marks` keeps the marks.

blaze-cli engine_bodies.rs opens one `Records` per stretch (`names_records_with_snapshot` in
`run_bodies`). It opens another after a body's frame refusal. So the demand is once per stretch
that lowers a head or body written by that call: 12.6 stretches per key on average.

What removes the repeat is a request-wide table of verified expansion texts, verified once (rule 74)
and lent to every stretch's items memo, the way `use_solved_texts` fills `seen`. It is half on this
side (a `Records` that takes the lent table) and half on the store side (the table itself, filled
from the snapshot).

One bypass is left: front2 program/lazy/seam_rules.rs `recorded_call_origin` calls
`Load::expansion` directly. It has no callers today.

## Witness records: an async body's auto traits (2026-11-20, spec; tempo's 156)

tempo's check on 2026-11-20 left 156 bodies unchecked over async futures' auto traits
(blaze-int notes reports/tempo-correct-1120 not-checked-by-cause.txt, the first six rows). They
are one gap. This section is its end state. The machinery it finishes already exists:
`Checked::stalled`, front2 stalled_goals.rs, exact_interior.rs, opaque_auto.rs, and the
coroutine row's `InteriorIs`.

### What the witness is (the rule)

- An async block's future, and an `async fn`'s root block, is `Send`, `Sync` or `Unpin` exactly
  when it holds for everything the state stores.
  - The Reference, "Async blocks": the future is "roughly equivalent to an enum with one variant
    per await point, where each variant stores the data needed to resume from its corresponding
    point".
  - "Auto traits": an auto trait holds for a type when it holds for everything the type holds.
- What the state stores is the block's WITNESS. rustc computes it from MIR after borrowck, with
  regions erased (`mir_coroutine_witnesses`). It has three parts:
  1. Each capture, typed by its mode: a by-value capture is its place's type, and a by-ref or
     by-mut capture is `&T` or `&mut T` of its place.
  2. Each local saved at some suspension: maybe storage-live there, and either live by use or
     maybe borrowed since its `StorageLive`. This is blaze-flow suspend.rs `saved`, D-A1, with
     the same rule as rustc's `MaybeBorrowedLocals` and `MaybeRequiresStorage`.
  3. Each awaitee, which is held across its own `.await`.
- The witness's types are region-ERASED. No part of the rule reads a region. "Maybe borrowed" is
  a fact of where a `&` was taken, not of how long a region lives.

### The cause of the 156

The witness is computed today only by the borrow check's lowering (`lower_async` over
`NestedFacts`). That lowering relates every call's regions, and it refuses whatever it cannot
relate:
- `CallHiddenInput { why: NotLaid }`
- `CallOutlivesUnread`
- `CountUnknown { what: OpaqueCaptures }` at a call of an `async fn(self)`
- `RegionsUnread` of a callee head that is not laid

A refused block then has no witness:
- In its own body, this is the 45 "no lowered async block supplied its captures and saved
  locals" (stalled_goals.rs). Every note quotes one of the refusals above under "borrows not
  checked".
- Filed as a superset (`FiledInterior::Superset`), it is the 65 "the auto trait of another fn's
  future needs its witness record" in every body that asks.
- The 6 `INTERIOR_GAP` rows and the 4 "checked output and stored types of coroutine" rows are
  the same superset, reached through an unsizing or a revealed opaque.

So 120 of the 156 wait on a REGION refusal for a REGION-FREE fact. (This is from the notes'
quoted refusals. Each refused block's form has not been counted.)

### The design

**W1. A witness lowering with regions erased, at the existing seam.**
- blaze-flow-lower's call rule is a type parameter at one seam already: one_call.rs `CallRule`,
  with `OldPaths` and `OneRule` (rule 78, D156).
- Add a third instantiation, `Erased`. It makes each call's statements (operands moved or
  copied, destination written, the call's unwind edge) and relates NO region.
- Under `Erased`:
  - `npos` counts no position for any type. So the counting refusals (`TypeNode`, `UnknownAdt`,
    the opaque and alias counts) cannot arise, and no origin vector is laid.
  - `Facts::sig` is not read (coroutine.rs, await_call.rs and lib.rs read it only to relate).
- The flow's locals, statements, `StorageLive`/`StorageDead`, borrows (`Rvalue::Ref` keeps its
  place, with no origin) and `Suspend` points are the same as under `OldPaths`.
- The oracle is the existing one (oracle.rs): on every fixture body both lower to equal
  statements once origins are dropped, and `saved` gives equal bits.
- Planted control: an `Erased` that drops one `StorageDead` gives a different `saved`.
- `lower_async::<Erased>` is what exact_interior.rs and stalled_goals.rs call for the witness.
  The borrow check keeps `Production`.
- The same lowering cannot serve both, by design: the borrow check must refuse what it cannot
  relate (rule 3), and the witness has nothing to relate.
- The one remaining refusal of the witness lowering is a FORM it does not lower (`InlineAsm`,
  `StaticMut` and the like). Each stays named.

**W2. The witness is a record of the body that owns it, made by that body's own check.**
- It is the coroutine row `opaque_auto.rs` files (`Store::hidden`), keyed like the body: owner
  plus the coroutine's ordinal (rule 65). Its content is `CoroutineFacts`: output, interior
  tuple, and `is`.
- With W1, `is` is `Exact` for every block whose forms lower.
  - `Superset` remains only for a block with a form the witness lowering does not take.
  - `CapturesUnknown` remains only until W4.
- A reader records `Question::Coroutine` with the answer's content (`CoroutineFacts:
  ContentHash`, as today). Its read list covers the witness it was checked against.
- Made once per body (rule 77). The flow that gives the witness is lowered once, under `Erased`,
  where the owner's clean check is filed. A later borrow check lowers under `Production` for its
  own purpose.

**W3. Cycles: records are types, evaluation is coinductive, and a Need never deadlocks.**
- A witness is TYPES. Making it needs the owner's typing and its flow, never an auto-trait
  answer. So the owner's check files its witness BEFORE it decides its own stalled auto-trait
  goals: `check_deferring` stalls them (`Checked::stalled`) and `decide_stalled_goals` decides
  them after the file.
- The cycle case, `async fn a() { spawn(b()) }` and `async fn b() { spawn(a()) }`, runs as
  follows:
  1. a's check types a, stalls `b-future: Send`, and files a's witness.
  2. Deciding the stalled goal is a `Need` on b's witness (D161: a Need parks, never stops).
  3. b's check types b, stalls `a-future: Send`, and files b's witness.
  4. Deciding b's goal reads a's witness, already filed.
  5. a's decision resumes.
  No record waits on a decision.
- The EVALUATION over witnesses may cycle: `Send(a-fut)` needs `Send(b-fut)`, which needs
  `Send(a-fut)`. That cycle is coinductive, as rustc's auto traits are. The solver's existing
  coinductive auto-trait cycle answer holds it (blaze-solve, as for a recursive ADT). This is
  bounded by the solver's depth cap (rule 18).
- opaque_auto.rs's `HiddenFail::Cycle` stays for the one real cycle: an auto trait of a fn's OWN
  opaque asked in its own body before its witness exists (rustc E0391's class). With W3's order
  that is only a goal the typing itself needs, never a stalled one.

**W4. Closures and captured places, the same way.**
- The 17 `ClosureCaptureWitness` rows: `TyKind::Foreign(def)` is a closure environment whose
  capture tuple is another body's (blaze-solve builtin.rs `closure_upvars`).
- The 19 `NeedsCapturePlaces` rows: an async block whose captures are places (`self.eth.api`
  through `&self`, RFC 2229 disjoint fields), not whole bindings.
- Both are the CAPTURE part of the witness. Its home is the closure rule's walk (blaze-typeck
  rules/closure.rs `visit_for_captures`), extended to places by the Reference, "Closure
  expressions", "Capture modes", "Capture precision":
  - a place is a binding and its field and deref projections;
  - it is truncated at a raw-pointer deref, at a union field, and at a `move` closure's deref of
    a reference;
  - its mode is the strongest use.
- It is filed with the closure's or block's coroutine row as the row's capture column, and read
  by `closure_upvars` through `Facts` as the coroutine row is (one record, one key: owner plus
  ordinal).

**W5. Hidden types across functions: no new mechanism.**
- The 6 "hidden type of the `impl Trait` type of fn `recv`/`fetch_l1_blocks`/`handle_ws_session`"
  rows are already that record: opaque_auto.rs files `Checked::hidden` from the defining body's
  clean check. What failed is that body's own check:
  - `fetch_l1_blocks`: `ClosureCaptureWitness`, which is W4.
  - `recv`: one gap.
  - `handle_ws_session`: a lowering refusal of `tokio::select!`'s `mod` in a block, which belongs
    to the lowering lane.
- They fall when their causes do.

### Pieces, in order, with the expected fall

| piece | files | rows it should clear | check |
|---|---|---|---|
| W1 `CallRule` `Erased` + `lower_async::<Erased>` at the two witness sites | blaze-flow-lower one_call.rs, lib.rs `npos`, coroutine.rs, await_call.rs; front2 exact_interior.rs, stalled_goals.rs | 45 + 65 + 6 + 4, where the refused form is a region form | tests/front2_witness_records.rs `async_futures_are_send_by_their_witnesses` (`spawn_worker`, `local_block`); the oracle's equal-shape test |
| W2/W3 order: file the witness before deciding stalled goals | front2 check.rs `check_rounds_as`, opaque_auto.rs `file` | the cycle cases among the 65 | the same test (`ping`, `pong`) |
| W4 capture places and the closure capture column | blaze-typeck rules/closure.rs, rules/async_fn.rs; blaze-solve builtin.rs `closure_upvars` | 19 + 17 | the same test (`boxed_field`, `returned_closure`) |
| W5 | none | 6, as their causes fall | the same tests |

Written 2026-11-20: this spec, and the acceptance tests (tests/front2_witness_records.rs; they
fail until W1 and W4 are built). W1 touches blaze-flow-lower, which is not this lane's crate.
Its edits are listed for its owner, or this lane builds it once granted.
