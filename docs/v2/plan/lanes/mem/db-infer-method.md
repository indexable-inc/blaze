---
status: plan
summary: mem-db-infer-method's plan to move method resolution (the probe, confirm, autoderef), the trait and inherent impl tables, and hir-ty's traits, specialization, opaques, drop, inhabitedness and representability queries onto blaze-mem; where each frame comes from, which temporaries move, how each memo result is made once at exact size, what still allocates and why, and the speed-of-light bound.
date: 2026-10-29
---
# mem-db-infer-method: method resolution onto blaze-mem

Part of `mem-db-infer` (`docs/v2/plan/lanes/mem/db-infer.md`, rules R1 to R8) under `mem-db`
(`docs/v2/plan/lanes/mem/db.md`). Files, in `crates/hir-ty/src/`: `method_resolution.rs`,
`method_resolution/{probe,confirm,frozen}.rs`, `autoderef.rs`, `traits.rs`, `specialization.rs`,
`opaques.rs`, `drop.rs`, `inhabitedness.rs`, `representability.rs`. Lane db owns them all and lane
items most of them, so every change is a request, written as exact before/after code in
`v2/requests/mem-db-infer-method.md`.

## Where the frame comes from

- Inside inference, a probe or confirmation takes the `InferenceContext`'s frame (R2): the
  `MethodResolutionContext` gains `frame: &'a Frame`, filled from `self.frame`; `ConfirmContext`,
  its lowerer and `InferenceContextAutoderefCtx` gain `'f`.
- Outside inference, `crates/hir`'s two constructions of the context, `impl_work`,
  `where_predicate_must_hold`, `impl_unconstrained_params`, `has_drop_glue` and the two
  inhabitedness checks open `frame::task(Tag::Infer, ..)` (R1). The last three also get `_in`
  forms that take the caller's frame, for MIR building and match checking.
- Each tracked query that has temporaries opens `frame::current` at the top of its body and
  passes the frame down. Every tracked query gets `mem_tag = crate::db::INFER_TAG` (R6).

## What moves

- **Probe** (per method call and per path): the four candidate lists, `impl_dups`, the autoderef
  steps, `TraitValueItems`' runs and completion's `probe_all` map go into frame `Vec`s and sets.
- **Autoderef** (per deref chain): the steps become an inline `ArrayVec` of 20. The walk refuses
  a 21st step, so it never allocates.
- **Confirm**: the receiver's adjustments stay in the frame and go to `write_expr_adj` as a slice
  (expr's X1/X2).
- **Impl tables** (per crate and per block): one frame builder per table takes flat `(group, id)`
  records. Groups are numbered as first seen. `finish` sorts them and lays out every column once
  at its size with a counting pass (sort-and-CSR). Frozen reads push into the same builders. The
  tables come out `PartialEq`-equal to the old construction.
- **Other queries**: frame `Vec`s, then one exact `Box` (no grow-then-shrink). `macro_definitions`
  becomes two flat columns instead of a `Box` per macro. `rpit_hidden_types` is sized once.
  `params_in_repr` builds its words in the frame and writes one exact `ThinVec`.

## What still allocates on the global heap

The solver's types (`QueryResponse` per probe step, `OriginalQueryValues`,
`PredicateObligations`, `elaborate`, `ObligationCtxt`) belong to mem-db-solver. Base-db's
`transitive_deps`, syntax's `LexedStr` and parses, error values (`MethodError::Ambiguity`) and
results owned by callers also stay. The request file lists every remaining hit with its reason.

## Speed-of-light bound

- **Per probe:** about 1 KB written to the frame in about 6 bumps (about 12 ns). The same work
  was 8 to 12 `malloc`/`free` pairs (about 200 ns).
- **Per 1,000-expression body** (about 250 probes): about 250 KB of frame and about 3 µs of
  allocation, where it was 40 to 75 µs. The probe's solver goals (µs each) and the solver's
  query responses are what remains.
- **Per crate's impl tables** (std: about 8,000 trait impls in 5,000 rows): about 400 KB of frame
  records, one sort of 4-byte indices, and about 140 KB of result columns written once. That is
  under a millisecond, against about 15,000 grown `Vec`s and maps before.
- **The bound** is writing the result columns plus one pass over the records.
