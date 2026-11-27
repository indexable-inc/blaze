---
status: current
summary: "The per-body type checker (crates/blaze-typeck): one body in, one record out, a pure function that returns its result or the facts it lacks; every fact from outside the body read through one sealed recorder, so a result cannot depend on what its read list does not hold; the walk an explicit work stack in rustc's order; results and diagnostics in portable columns; pattern types, closure identity and async witnesses as records."
---
# The type-check driver

`crates/blaze-typeck` checks one body: the walk over its expressions and patterns, expectations
and coercions, method and path resolution inside bodies, closures and captures, and the
obligations it registers. Types are blaze-ty's; trait goals go to blaze-solve. This is where
incremental bugs live (a result that depended on something it did not record), so the design is
built around the read list. The name `blaze-check` is the model checker's.

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
pub fn check<'a, W: World, C: TypeCtx>(
    body: &BodyView<'_>, world: &W, ty: &mut C, hints: &Needs<'_>, alloc: Alloc<'a>,
) -> Result<Outcome<'a>, CheckError>;

pub enum Outcome<'a> { Done(Checked<'a>), Need(Needs<'a>) }
```

- A pure function of the body, the world's answers and the hints. No global state, no locks, no
  thread-locals, no interior mutability shared across bodies. Two runs with equal inputs give
  equal bytes (rule 25), whatever the type context numbered its variables.
- `Need(questions)`: the world had no answer yet for a fact (a callee's signature not yet made, an
  impl bucket not yet fetched). The caller gets those records (reading them, or running their
  tasks) and calls `check` again from the top, with nothing of the run kept. A world that
  answers `Missing` gives the answer's stand-in (`Hole`, `Holed::hole`), so one round finds every
  record the body lacks; a run that met a hole ends `Need` and makes no result. Restarting is
  what the tree does; end-state.md D161 decides that a lacking task parks and resumes instead,
  nothing starting over.
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

The rest (`LangPath`, `ConstHolds`, `ConstParams`, `Closure`, `Coroutine`, `AssocGenerics`,
`TypeAlias`, `FieldDefaults`, `MemberVisible`, `MemberUnstable`) are listed in world.rs's
`Question`.

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

Every question reads one of two kinds of record (`Question::record_kind`): a body record
(`Opaque`, `Const`, `ConstHolds`, `Closure`, `Coroutine`), made by another body's check, or an
item record (every other question), made by the item layer from items' text alone.

**Misses made rare by order.** A body that misses a record stops and runs again from the top
once the driver has fetched it (D161 replaces the restart with a park). The order that makes a
miss rare is a gate: a body's check is queued only after the item records its scope can name
have settled, so its `Need` is only ever a body record. `RecordKind` is what the driver reads to
tell which questions a body may still wait on: `Body` ones.

A body with no record (empty hints) runs a syntactic prefetch before
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
(item records settled first) and `KindWithGoals` (a ground goal's miss is its own goal task).
R is wasted over total work, reported against `R_THRESHOLD` (0.10). The property `the_ask_log_predicts_reentry` shows the simulation equals real re-entry
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

All state is plain data in columns of `u32` indices and `Ty` handles, so a run stopped at
`Need` can be kept and resumed (D161) with no fiber.

## Results: one record in columns

`Checked<'a>` (columns.rs) holds, among other columns, all `Portable` (items by `ItemKey`, types in the record's own type table):

- `types`: a `PortableTys` table, hash-consed by structure, so equal types are equal indices and
  the bytes depend on structure and walk order only, never on a context's handles;
- `expr_ty` and `pat_ty`: one `PTy` per expression and pattern; `local_ty` per binding;
Sparse facts are `blaze_own::Soa` tables sorted by expression, so a lookup scans one column:

- `adjusts`: per expression, consecutive `Adjust` steps (`NeverToAny`, `Deref(builtin or
  overloaded)`, `Borrow(mutbl, two_phase)`, `ReifyFnPointer`, `Unsize`), each with its target;
- `callees`: per call and method call, the item and its generic arguments;
- `fields`: per field access, the variant and field index (tuples too);
- `captures`: per closure, the captured places and how (by ref, by mut ref, by value);
- `diags`: typed `Diag { code, at: Range, .. }`, item-relative;
- `reads` and `needs`: the read list and the questions to fetch next time.

## Diagnostics are values

The driver never panics on a user's error and never stops for one. A type error records a
`Diag` and gives its expression the error type, which unifies with everything and silences
errors that follow from it, as rustc's `ty::Error` does. Codes are rustc's (`E0308`, `E0282`,
`E0599`, `E0609`, `E0061`, `E0614`, `E0606`, ...), so a person's search finds the same page.

## `TypeCtx`: what is asked of blaze-ty

The crate defines the traits it needs; `infer::InferCtxIn` implements them over blaze-ty and
blaze-solve, and tests over a toy type language (toy.rs). Each item names its rustc counterpart:

- `Ty`, `TyList`: 4-byte handles over blaze-ty's `Ty` and `List`.
- `TyView`: a copyable view of one level of a type (`Bool`, `Char`, `Int`, `Uint`, `Float`,
  `Str`, `Never`, `Tuple`, `Ref`, `Ptr`, `Array`, `Slice`, `Adt`, `FnDef`, `FnPtr`, `Closure`,
  `Param`, `Alias`, `Infer`, `Error`, and more: ty.rs); blaze-solve's `TyKind` through
  infer/terms.rs, regions erased as rustc's typeck sees them.
- `mk`, `view`, `mk_list`, `list_get`: the interner.
- `fresh(kind)`, `shallow_resolve`, `resolve`, `unify`: the `InferCtxt` table operations.
- `snapshot`, `rollback_to`, `commit`: `InferCtxt::probe` and `commit_if_ok`; the driver wraps
  them in `core::mem::DropGuard` so an early `?` rolls back.
- `assume(clause)`: the body's param env.
- `register(obligation)`, `select_where_possible(reads)`, `select_all(reads)`, `next_error`:
  rustc's `FulfillmentCtxt`; here blaze-solve's `Fulfill`, reading impls through `Reads`.
- `may_hold(obligation, reads)`: `predicate_may_hold` in a probe, for the method probe.
- `normalize(trait, self, args, name, reads)`: an alias normalized now if selectable, for
  overloaded deref and operator outputs.
- `closure_hint(var, [Fn, FnMut, FnOnce], Output)`: the inputs of the first pending
  `var: Fn*<(..)>` obligation and the term of a pending `Output` projection, for a closure's
  expected signature (`deduce_closure_signature`).

The driver's **portable type form** (`PNode`/`PTy`, portable.rs) is: a table of
nodes with children before parents, items by key, generic parameters by index, and a hole
(`Infer`) only in a body's written types; and a content hash of it (`ContentHash` here, FNV-1a
until blaze-ids' `ContentId` is at zero third-party dependencies).

## Rules, by file

Each file under `src/rules/` begins with the rustc file it follows (and the Relax file that maps
it). They cover literals and paths (with turbofish and type-relative paths), blocks, `let` and
statements, calls of fn items and fn pointers (arguments in rustc's two passes, closures last
after a selection), method calls (the probe: steps by autoderef with the array-unsize step last;
per step by value, then `&`, then `&mut`; inherent before traits in scope), fields and tuple
indexing through autoderef, operators (the builtin path for scalars before the trait), borrows
and dereference, casts (deferred), `if`/`match`/`loop`/`break`/`return` with divergence and the
never type, patterns with default binding modes (and edition 2024's refusals), struct, tuple
and array expressions, coercions (`Coerce::coerce`'s order: unsize, then by target pointer,
then by source fn item or pointer, then unify; `CoerceMany`), integer, float and never
fallback, writeback, closures' expected-signature deduction from pending `Fn*` obligations and
capture analysis as its own pass, associated items and projections through the solver;
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

## Floors (rule 67)

A body's check is linear in its nodes plus the solver's work: one frame push and pop per node,
one column write per result, no allocation per node (every column is one buffer from the task's
allocator), and the reads it makes. The floor for a repeat check with nothing changed is zero:
the record's read list stands and the driver does not run. Its ratio on std's bodies is not
measured.

## Forms from blaze-lower

blaze-lower (crates/blaze-lower) lowers bodies with rustc's desugarings and converts them to this
crate's `BodyView` in `typeck.rs`'s `to_typeck`; a form `BodyView` has no place for is refused by
name (`Refused::Form`), and a form the checker does not check yet is `CheckError::Unported`.
Bodies in blaze-lower's forms are checked in suite/lower_forms.rs.

## Trait objects

`dyn` follows the Reference, "Trait object types", "Dyn compatibility" and "Unsized coercions":
- (a) Type: `PNode::Dynamic(list)` / `TyView::Dynamic` over predicate nodes, as blaze-solve's:
  `ExTrait(trait, args after Self)` first (absent for `dyn Send`), then `ExProj` sorted by
  (trait, name), then `ExAuto` sorted by `ItemKey`, repeats dropped. One constructor
  (`PortableTys::dynamic`) makes the form, so order and repetition as written never reach a
  key; two principals, or two bindings of one (trait, name), are refused (E0719).
- (b) What it implements: the principal, its supertraits and the listed auto traits; never
  `Sized` (the solver's object candidate and sized rule).
- (c) Method calls: at a `Dynamic` autoderef step the principal's and its supertraits' methods
  are candidates whether or not the trait is in scope; a `where Self: Sized` method is refused by
  its `dyn: Sized` clause.
- (d) Coercions: `coerce_unsized` asks the solver `Unsize` of the pointees for a `dyn` pointee and
  `CoerceUnsized` for an ADT such as `Box`; an auto trait may be dropped, never added.
- (e) Dyn compatibility: computed in the checker (dyn_compat.rs) from the members a world states
  for the trait; each failure is `DynIncompatible { trait_, member, rule }` at the `dyn` type's
  range.
- (f) Regions are erased, so `Dynamic` has none; default object lifetimes are the borrow
  checker's.
- (g) A method or `Fn*` call on an object resolves to the trait's method with `Self` = the
  `Dynamic` type in its recorded args: that is what makes the call dynamic for the code builder.

Tests: suite/dyn_objects.rs.

## Pattern types

`pattern_type!(T is PAT)` (a builtin type macro, core/src/pat.rs) is what the pinned core writes
in num/niche_types.rs (`NonZeroU8Inner(u8 is 1..)`, `NonZeroI8Inner(i8 is ..0 | 1..)`,
`Nanoseconds(u32 is 0..=999_999_999)`, `NonZeroCharInner(char is '\u{1}'..='\u{10ffff}')` and
their kin) and in `*const T is !null` (ptr/non_null.rs). Its patterns are an integer or `char`
range (`a..=b`, `a..`, `a..b`, `..b`, ends literals or paths to consts), an or of ranges, and
`!null`.

**Meaning.** `T is PAT` is the values of `T` that match `PAT`, with `T`'s layout. Each pattern
core uses is a union of inclusive ranges of the base's bits; `!null` is `1..=MAX` of the
pointer's address. So the node is `PNode::Pat(base, ranges)` (`TyView::Pat`, blaze-solve
`TyKind::Pat`, blaze-ty's term): `ranges` holds const terms in pairs, `start ..= end` each,
`start > end` meaning wrapping. The constructor canonicalizes: ranges sorted, merged when they
touch, and a run that ends at the base's maximum merged with one that starts at its minimum into
one wrapping range, so `i8 is ..0 | 1..` is `1 ..= -1`, the form layout needs.
- **Equality** is structural on the canonical form. This accepts a little more than rustc (`u8
  is 1..` equals `u8 is 1..=255`), never less; coherence could only see two impls overlap where
  rustc does not, and core writes no such pair.
- **Relation to the base:** none implicit. Core goes `T -> T is PAT` and back only through
  `transmute`, which needs equal sizes. No coercion, no field, no method of the base; a pattern
  type's impls are its own (pat.rs's).
- **Layout:** the base's size and alignment with the canonical range as its valid range, so
  `Option<NonZero<u32>>` is 4 bytes.
- **Constants in a pattern** (`HALF_USIZE`, `u32::MAX`) are evaluated by the front end that
  expands the macro; the node holds values only (or a const parameter, for the extension).

blaze's extension (extensions.md: `T is PAT` written directly, over enum variants, with
narrowing) is the same node with `ranges` over the base's variant index when the base is an
enum (`Option<T> is Some(_)` is `1..=1`).

## A closure's identity

The Reference ("Closure types"): a closure expression produces a value of a unique, anonymous
type; the `Fn*` traits it implements follow from how it uses its captures; a closure in a
generic item is generic over the item's parameters. The portable form is `PNode::Closure {
owner, ordinal, args }`:

- `owner`: the `ItemKey` of the item whose body holds the closure expression. The owner is
  always the item: a closure inside a closure is the item's too.
- `ordinal`: the closure's index among the body's `Expr::Closure`s in `ExprId` order, the
  lowering's order of the text. An edit may renumber the closures after it; any edit in the body
  changes the owner's body key, so no record keyed by the old text survives it. Never an
  `ExprId` in a shared record.
- `args`: the owner's generic arguments in scope, so `f::<u8>` and `f::<u16>` make two closure
  types; instantiation substitutes them like any argument.

The signature, the kind (`Fn`, `FnMut`, `FnOnce`: the strongest it implements) and the captures
are facts, not the type's identity: decided when the owner's body is checked and stored in its
`Checked::closures` (one row per closure, by ordinal) and `Checked::captures`. Another body that
meets the type asks `Question::Closure(owner, ordinal)` (a body record, as an opaque's hidden
type) and gets `ClosureFacts { kind, sig, types }`, or a typed gap when the owner is unchecked.
The checker views a foreign closure as `TyView::ClosureOf { owner, ordinal, args, sig, kind,
upvars }`; the solver sees rustc's layout `[args.., kind, sig, ()]`, so its `Fn*` rules read the
signature and the kind from the type. Inside the owner's body a closure is `TyView::Closure`
while its signature is inferred, and writeback names it by ordinal.

A capture's mode is `ByValue` for every capture of a `move` closure and for a non-`Copy` value
used by value, else `ByMutRef` for a mutable use, else `ByRef`; the closure's kind is the
strongest use (moved out: `FnOnce`; mutable: `FnMut`; else `Fn`). Captures as places (RFC 2229:
fields and derefs of a binding, truncated at a raw-pointer deref, a union field and a `move`
closure's deref of a reference) are `capture_places.rs`. Tests: suite/closure_identity.rs.

## Witness records: an async body's auto traits

An async block's future, and an `async fn`'s root block, is `Send`, `Sync` or `Unpin` exactly
when that holds for everything its state stores (the Reference, "Async blocks" and "Auto
traits"). What the state stores is the block's WITNESS, region-erased, as rustc's
`mir_coroutine_witnesses`:
1. each capture, typed by its mode (`T`, `&T` or `&mut T` of its place);
2. each local saved at some suspension: maybe storage-live there, and live by use or maybe
   borrowed since its `StorageLive` (blaze-flow suspend.rs `saved`, rustc's
   `MaybeBorrowedLocals` and `MaybeRequiresStorage`);
3. each awaitee, held across its own `.await`.

- **W1, the witness lowering relates no region.** blaze-flow-lower's call rule is a type
  parameter (one_call.rs `CallRule`); its `Erased` instantiation makes each call's statements and
  relates no region, so no region refusal stops a witness. exact_interior.rs and
  stalled_goals.rs lower under it; the borrow check keeps its own rule, which must refuse what it
  cannot relate. The one refusal left is a form the lowering does not take (`InlineAsm`,
  `StaticMut`), by name.
- **W2, the witness is a record of its owner.** The coroutine row opaque_auto.rs files, keyed by
  owner and ordinal; its content is `CoroutineFacts` (output, interior tuple, `InteriorIs`). A
  reader records `Question::Coroutine` with the answer's hash.
- **W3, cycles.** A witness is types: making it never needs an auto-trait answer. So the owner's
  check files its witness before it decides its own stalled auto-trait goals (`Checked::stalled`,
  `decide_stalled_goals`). `async fn a() { spawn(b()) }` with `async fn b() { spawn(a()) }`:
  each files its witness, then decides its goal by reading the other's; the evaluation's cycle
  (`Send(a-fut)` needs `Send(b-fut)` needs `Send(a-fut)`) is coinductive, as rustc's auto traits
  are. opaque_auto.rs's `HiddenFail::Cycle` is left for an auto trait of a fn's own opaque asked
  in its own body (rustc E0391's class).
- **W4, closures.** The capture part of a closure's or block's witness is the closure rule's
  walk (rules/closure.rs, capture_places.rs), filed with its coroutine row and read by blaze-solve
  builtin.rs `closure_upvars`.

Tests: crates/blaze-ladder/tests/front2_witness_records.rs, flow-lower coroutine_tests.rs.
