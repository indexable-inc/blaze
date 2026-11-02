---
status: plan
summary: mem-db-infer-closure's plan to move closure signature deduction, capture analysis and the expression-use visitor of hir-ty's inference (infer/closure.rs, closure/analysis.rs, closure/analysis/expr_use_visitor.rs) onto the inference run's frame; places and their sources become frame values while analysing, ClosureData is built once at exact size (MinCaptures and boxed slices in place of FxIndexMap, Vec and SmallVec), every reader, what stays global and why, the tests and the speed-of-light bound.
date: 2026-10-29
---
# mem-db-infer-closure: closure analysis onto blaze-mem

Part of `mem-db-infer` (`docs/v2/plan/lanes/mem/db-infer.md`, rules R1 to R8) under `mem-db`
(`docs/v2/plan/lanes/mem/db.md`). Paths, in `crates/hir-ty/src/infer/`: `closure.rs`,
`closure/analysis.rs`, `closure/analysis/expr_use_visitor.rs`. The exact changes, with before and
after code and their lines, are in `v2/requests/mem-db-infer-closure.md` (entries C0 to C2, E1 to
E8, A0 to A8, and interfaces X1 to X5 for `infer.rs`, `persist.rs` and `lib.rs`). This note is
the design.

## The shape

Closure inference has two halves. Signature deduction (`closure.rs`) runs while a closure
expression is inferred; it keeps no lists of its own: the obligations and bounds it reads come
from the table (`obligations_for_self_ty`), the elaborator and lowering, which are other parts'.
Its one temporary, the supplied argument list, goes straight into `mk_fn_sig`'s interning.

Capture analysis (`analysis.rs` with the visitor) runs once per closure after fallback, inside
`infer_query`. It builds places (a base and a list of projections), source stacks (which
expression produced each projection), captured uses, a minimum-capture set per root variable
and fake reads, and stores the last three in the closure's `ClosureData`, part of the memoized
`InferenceResult`. Today the same types (`Place` with a `Vec`, `SmallVec` of `ThinVec` stacks,
`FxIndexMap` of `Vec`s) serve both the analysis and the result, so every temporary is a global
allocation and the result is grown, then shrunk in `resolve_all`.

The split: the analysis gets frame types and the result keeps owned types, built once.

| analysis (frame, `'f`) | result (owned) |
|---|---|
| `PlaceBuf<'f>`: projections in a frame `Vec` | `Place`: `Box<[Projection]>` |
| `SourceStack<'f>`: 1 or 2 inline, else a frame `Vec` | `CaptureSourceStack`: 1 or 2 inline, else a `ThinVec` at its length |
| `Sources<'f>`: one stack inline, else a frame `Vec` | `Box<[CaptureSourceStack]>` |
| `CaptureInfoBuf<'f>`, `CapturedPlaceBuf<'f>` | `CaptureInfo`, `CapturedPlace` |
| `MinCapturesBuf<'f>`: roots in a frame `Vec`, a frame `hashbrown` index of `u32` (R3) | `MinCaptures`: `Box<[(BindingId, u32)]>` and `Box<[CapturedPlace]>` |

So `Place.projections` can be, and is, a frame value while analysing and owned only in the
result. Results are made at three points of `analyze_closure`: `min_captures` right after the
last `compute_min_captures` call (before the coroutine-closure part and the upvar tuple read
them), and `fake_reads` and `kind_origin` where they are stored today. Each conversion is a slice
copy at its length, so the result makes exactly the allocations `ClosureData::clone` makes,
which the parent's budget test compares. `resolve_all` then folds the result in place (a
`Box<[T]>` folds through `Vec`'s in-place fold) and shrinks nothing.

The only reverse direction is `walk_captures`: a nested closure's result becomes the enclosing
closure's uses, so its places and sources are copied into the frame.

## `MinCaptures`

`FxIndexMap<BindingId, Vec<CapturedPlace>>` is replaced by two blocks: every capture, root by
root, and each root with the end of its captures. Readers ask for what they used: `values()`
(each root's `&[CapturedPlace]`), `iter()` and `&MinCaptures` (pairs), `len()` (roots),
`flattened()` (the upvars in order, what `closure_min_captures_flattened` and MIR want). No
reader outside `infer.rs` and `persist.rs` changes; the request lists each one.

Its `Eq` and `Hash` are derived and so order-sensitive. The order of `min_captures` is the order
of the closure's upvar tuple and of MIR's captures, so it is part of the value; `IndexMap`'s `==`
and the `hash_unordered` it needed ignored it.

Persist's format is unchanged. The decoder learns the number of captures only after reading
every root, so it gathers them in the frame and copies them once.

## Fewer copies

Besides moving to the frame, the analysis stops copying what it owns:

- the delegate destructures each `PlaceWithOrigin` it is given instead of cloning its place (and,
  in `borrow`, its sources);
- `walk_pat` passes the scrutinee on instead of cloning it, copies a pattern's place for a
  discriminant read only when the read happens and the place is used after it, and a leaf
  pattern hands its place to the callback; the last arm of a `match` takes the scrutinee;
- `process_collected_capture_information` moves the captured uses instead of cloning each, and
  `compute_min_captures` truncates a removed descendant in place (`retain_mut`) instead of
  cloning it, and no longer removes and reinserts the closure's data;
- `normalize_capture_place` returns a place with nothing to normalize as it is, on exactly the
  condition `normalize_with_universes` does, so it skips a copy and a `FulfillmentCtxt` per
  captured use.

What is copied still is what the algorithm needs twice: a scrutinee for its fake read and each
arm, a place for each subpattern, alternative and adjustment. A copy of a place without
projections and with short stacks allocates nothing.

## What stays global, and whose it is

The result's own allocations (R4). `obligations_for_self_ty`'s `SmallVec<[_; 4]>`, which spills
on real closures, and `remove_deferred_call_resolutions`' `Vec` are mem-db-infer-ctx's; the
elaborator's stack and set, `PredicateObligations`, `Tys::new_from_iter`'s `SmallVec<[_; 8]>`
and the normalizer's `FulfillmentCtxt` are mem-db-solver's; `lower_closure_params`' `Vec` is
mem-db-lower's. `sort_by` of one root's captures uses a 4 KiB stack buffer and allocates only
above about 39 captures of one variable; it stays because rustc's order depends on its
stability. The visitor's `SmallVec<[_; 5]>` copy of an expression's adjustments spills only above
5 adjustments on one expression (R3).

## Speed of light

On a 1,000-expression body with 10 closures over 300 of its expressions (the closure-heavy end of
real code), the temporaries are about 35 to 45 KB in about 345 frame allocations, reclaimed when
the run ends: about 0.7 µs at 2 ns each, against about 600 global allocations and frees (about
12 µs) and 60 `FulfillmentCtxt`s before. Categorising an expression costs at least one deep
resolve and usually a structural resolve, 50 to 100 ns; the temporaries add about 2.3 ns per
closure-body expression, 2 to 5% of the pass, so the pass is within 10% of its type work alone.

What remains is one frame block per place value that must exist at once. Keeping two projections
inline would save the 150 chain allocations (about 0.3 µs, under 1% of the pass) at the cost of a
branch on every projection access, so it is not done. The result is about 75 allocations per
such body, each made once at its size (before: about twice as many, with growth and shrink
reallocations), and a capture's result shrinks from about 96 to 56 bytes for every memoized
inference result.

## Tests

The existing closure-capture, MIR, borrowck and persist round-trip tests must pass unchanged;
they pin the order of captures, sources and fake reads. New: `MinCaptures` splits its block by
root and compares in order; `CaptureSourceStack::from_slice` and `try_from_fn` build at their
length. The parent's budget test gets a fixture with a capture through a deref, a guarded match on
an upvar, a nested closure, a coroutine-closure, and the same in edition 2018.
