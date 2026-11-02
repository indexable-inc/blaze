---
status: plan
summary: mem-db-infer-ctx's plan for the core of hir-ty's inference on blaze-mem (infer.rs, infer/unify.rs, infer/columns.rs, infer/persist.rs and the small infer files); InferenceContext and InferenceTables gain the run's frame lifetime, the query functions take the frame and tag, columns are built from frame builders once at exact size, write-back and fallback run in the frame, the persisted record is encoded on thread scratch and decoded in a task at exact size, what stays global and why, the tests and the speed-of-light bound.
date: 2026-10-29
---
# mem-db-infer-ctx: the inference core onto blaze-mem

Part of `mem-db-infer` (`docs/v2/plan/lanes/mem/db-infer.md`, rules R1 to R8) under `mem-db`
(`docs/v2/plan/lanes/mem/db.md`). The exact changes, each with its before and after code, are in
`v2/requests/mem-db-infer-ctx.md`; the files are lane db's, so none is edited here.

## What changes

- **The context.** `InferenceContext<'db, 'f>` holds `frame: &'f Frame`; every list and set it
  keeps while inferring (breakables, unreachable expressions, deferred casts and calls, the
  defined anonymous consts, the variables already reported) is a frame `Vec` or `FrameSet`. Every
  `impl` and use of the type gains `'f` in one mechanical edit.
- **The queries.** `infer_body_query` and `infer_anon_const_body_query` carry
  `mem_tag = crate::db::INFER_TAG` and read their frame with `frame::current`;
  `infer_query_with_inspect`, called outside any query, opens a `frame::task`. Their shared body
  is `infer_body_in(.., frame)`.
- **The tables.** `InferenceTables<'db, 'f>` are frame builders (`DenseMap`, `CsrBuilder`,
  `ExprOrPatMap`, frame index sets), most reserved at the body's expression and pattern counts;
  `freeze` builds each owned column once at its length. Tuple types are interned through a frame
  index rather than an `IndexSet`.
- **Traits in scope.** One frame set per block module that adds traits, borrowed by every method
  call inside it (no `Rc` clone, no refill per call). The probe takes a `TraitsInScope` that is
  either that set or a caller's `FxHashSet`, so `crates/hir` is unchanged but for the variant.
- **Write-back and fallback.** `WriteBackCtxt` and its resolver keep their unresolved variables
  and spans in the frame and no longer collect nested goals; diagnostics are built in a frame list
  and boxed once. Fallback's variable list and diverging map are frame forms; the diverging type
  variables are a `VarSet` on thread scratch.
- **Persistence.** Encoding writes into the thread's scratch buffers and copies each array once
  at its length. Decoding opens its own task, builds every column with one exact allocation, and
  refuses a diagnostic count larger than the record's words before allocating for it.

## What stays global, and why

`Diagnostics` keeps its `ThinVec` (the result keeps it, and it is empty for most bodies);
`trait_errors` is only filled on errors; `could_unify` and `could_unify_deeply` open no task, as
they put nothing in a frame (the solver keeps recycled per-worker storage instead); the solver's
`unresolved_root_variables` and `clone_opaque_types` lists are its own; the result's columns,
closure captures and diagnostics' payloads are owned output.

## Speed of light

A run must write its result once: about 25 owned columns, one global allocation each (about
0.5 µs at 20 ns each), and their bytes written once (about 40 KB for a 1,000-expression body,
about 2 µs at memory bandwidth). Everything else is a frame bump (about 2 ns) and one reset.
Counted from the code, the change removes about 300 global allocator calls per run of inference
on such a body (about 6 µs) and about 250 per decoded result (about 5 µs); the encoder allocates
nothing beyond the record's three arrays. Not measured yet: mem-db-infer's budget tests
(`tests/budget.rs`) measure the bytes once this lands.
