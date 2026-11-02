---
status: plan
summary: mem-db-infer-expr's plan to move the per-expression and per-pattern temporaries of hir-ty's inference (infer/expr.rs, pat.rs, coerce.rs, callee.rs, cast.rs, op.rs, place_op.rs) into the inference run's frame; which temporaries move and to what type, the interfaces they need from the other parts, the one entry point outside a query (could_coerce), what stays global and why, the tests and the speed-of-light bound.
date: 2026-10-29
---
# mem-db-infer-expr: expression and pattern inference onto blaze-mem

Part of `mem-db-infer` (`docs/v2/plan/lanes/mem/db-infer.md`, rules R1 to R8) under `mem-db`
(`docs/v2/plan/lanes/mem/db.md`). Paths, in `crates/hir-ty/src/infer/`: `expr.rs`, `pat.rs`,
`coerce.rs`, `callee.rs`, `cast.rs`, `op.rs`, `place_op.rs`. The exact changes, with before and
after code and their lines, are in `v2/requests/mem-db-infer-expr.md` (entries E0 to E17 and
interfaces X1 to X4). This note is the design.

## The shape

These files run once per expression and per pattern of every body, inside `infer_query` (or a
closure or anon-const run in it). They hold no memo of their own: everything they make is
either a run temporary or a value copied into the context's tables (ctx's, R5). So every
temporary goes into the run's frame, `self.frame` on `InferenceContext<'db, 'f>` (R2), and none
needs a tag or a memo shape.

Three changes carry most of it:

1. **Adjustment lists stop being boxed.** `write_expr_adj` takes `&[Adjustment]` (the table
   copies it in any case), so a coercion's list, autoderef's list (`adjust_steps_as_infer_ok`,
   made in the frame at exact length) and the one-element lists of operators and LUBs are a
   frame `Vec` or a stack array. Before: 1 to 2 global allocations per adjusted expression, and
   one per earlier arm on every LUB.
2. **`Coerce` and `CoerceMany` hold the frame.** `CoerceResult<'db, 'f>` carries
   `Vec<Adjustment, &'f Frame>`. `CoerceMany`'s pushed expressions are a frame `Vec` that a
   `match` sizes to its arm count; before, a `SmallVec<[ExprId; 4]>` spilled to the heap on a
   fifth arm.
3. **Call arguments fold in place.** `check_call_arguments` made up to four global `Vec<Ty>`
   per call: the formal list, its clone, and the copies from `fudge_inference_if_ok`'s two
   folds (`TypeFoldable for Vec<T>` re-collects), plus a clone for the fallback. A `FrameList`
   newtype (`TypeFoldable` folding in place) and a borrowed fallback leave 1 to 3 frame lists.

The rest: tuple element types and struct-literal and record-pattern field maps go into the
frame at exact capacity. The field maps are `hashbrown` maps keyed by `&Name` borrowed from the
database's `VariantFields`, so no name is cloned. Record patterns make their unmentioned-fields
list only for the diagnostic that owns it. `DeferredCallResolution<'db, 'f>` keeps its
adjustments in the frame. `check_legacy_const_generics` stops computing `generics()` for every
fn-item call and returns its indices from the frame. `cast.rs`'s auto-trait set is a short frame
list.

Not there, though the brief listed them: `coerce_unsized` has no queue (one `visit_proof_tree`),
patterns have no binding-mode stack (`PatInfo` is `Copy`), slice patterns borrow their
prefix/suffix from the store, and subpattern type lists are interned.

## The entry point outside a query (R1)

`could_coerce` (`pub`, from `crates/hir`'s `Type::could_coerce_to`, used by five
`blaze-diagnostics` lowerings) opens its own `frame::task(Tag::Infer, ..)`. It also stops doing
work its `bool` ignored: the private `coerce` behind it evaluated obligations and folded the
adjustments after the coercion's `?`, and `is_ok()` read only that `?`. So `could_coerce` runs
just the coercion, which gives the same answer for every input, and the `Resolver` folder goes.

## What stays global, and why

The solver's types, which mem-db-solver moves: `PredicateObligations` threaded through every
`CoerceResult`, the `InferCtxt` that `could_coerce` builds, `goal.candidates()`. Autoderef's
`steps` and obligations (mem-db-infer-method). The owned lists of diagnostics, made only on
error paths (`RecordMissingFields::missed_fields`, `unmet_goals` of the operator diagnostics,
`lookup_op_method`'s error list). Interned values (`Tys`, tuples, existential predicates). The
request file lists every remaining match with its reason.

## Tests

The existing inference tests hold the semantics (`crates/hir-ide/src/tests/` `coercion.rs` with
its recorded-adjustment annotations, `patterns.rs`, `simple.rs`, `regression.rs`,
`method_resolution.rs`, `consteval.rs`, `never_type.rs`; `blaze-diagnostics`'
`tests/bodies/members.rs` for field diagnostics). Two new `coercion.rs` tests pin what changed
size: a six-arm `match` of fn items, all reified (past the old four inline slots, every earlier
arm's adjustment rewritten), and call-argument expectations through the in-place fudge on the
plain and the tupled closure path. The frame peak of a cold inference is held by mem-db-infer's
budget tests (R8).

## Speed of light

On a 1,000-expression body (about 250 calls, 300 recorded coercions, 60 struct literals, 30
record patterns, 20 `match`es, 30 tuples), the bound for these files' temporaries is their
bytes written once: about 35 KB, mostly field maps (19 KB) and call input lists (8 KB), in about
1,500 frame allocations. At about 2 ns per frame allocation and L1/L2-resident stores, that is 4
to 7 µs per run. Before: about 1,800 global allocations and frees at 15 to 25 ns, 30 to 45 µs,
plus the folds' copies. Inference of such a body takes 1 to 5 ms, dominated by the solver, so
these temporaries go from about 1% of the run to about 0.1%.

The plan reaches the bound within 10%. Each list is made once at its final size wherever the
size is known: formal and expected lists, tuple lists, field maps, `match` arm lists, autoderef
lists. The others (coercion adjustment lists of 1 to 3, return and break lists) grow the frame's
top block in place and are freed in place when dropped on top. Writing adjustments straight
into the table instead would skip one copy of about 8 bytes per coercion, but coercions are
speculative (`commit_if_ok`), so that would need a rollback on the table. It is not worth it at
under 1 µs per run.
