---
status: plan
summary: The per-body type-check driver written new (crates/blaze-typeck): one body in, one record out, a pure function that returns its result or the facts it lacks; every fact from outside the body read through one sealed recorder, so a result cannot depend on what its read list does not hold; the walk an explicit work stack in rustc's order; results and diagnostics in portable columns. The type tables, the next-generation trait solver, MIR building and the borrow checker are kept; the old driver (hir-ty's infer and method_resolution) is the referee until every body of core, alloc and std agrees.
date: 2026-11-15
---
# The type-check driver, from scratch

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
- `diverging`: variables that received a `!` by coercion, for never-type fallback (`()` before
  edition 2024, `!` from it).
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

## Not decided here

- Whether a run stopped at `Need` should save its state instead of re-entering: decided by a
  profile of re-entries on reth and tempo.
- Regions: typeck erases them as rustc does; the borrow checker keeps its own.
