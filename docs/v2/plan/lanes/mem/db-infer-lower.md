---
status: plan
summary: mem-db-infer-lower's plan for type lowering (lower.rs, lower/, generics, utils, variance, dyn_compatibility, builtin_derive, effects) on blaze-mem; how TyLoweringContext and the batches reach their frame, which temporaries move, how each memo is built once at its exact size, and the speed-of-light bound.
date: 2026-10-29
---
# mem-db-infer-lower: type lowering onto blaze-mem

Part `lower` of mem-db-infer (`docs/v2/plan/lanes/mem/db-infer.md`, rules R1 to R8, which
program against mem-db's contract in `docs/v2/plan/lanes/mem/db.md`). The exact changes, with
code, are `v2/requests/mem-db-infer-lower.md`. They are requests, because the files are lane
db's.

## The context carries the frame

`TyLoweringContext<'db, 'a>` gets `pub(crate) frame: &'a Frame`, the last parameter of `new`
(R2). `'a` is already the borrow of the creator's resolver and generics, which the frame outlives.
It is fed by:
- a signature query: its `blaze_mem::frame::current` (R1);
- inference: `InferenceTyLoweringContext` passes its `InferenceContext`'s frame;
- an IDE caller outside queries: its own `frame::task(Tag::Infer)`.

`PathLoweringContext` reaches it through the context it holds.

Everything the context collects lives in the frame:
- diagnostics and defined anonymous constants (frame `Vec`s);
- `?Sized` types (a frame hashbrown set);
- the opaque types' bounds, and the elision stack with each signature's argument lifetimes;
- the binders' names, as one flat list with a start per binder in place of a `Vec<Name>` per
  binder.

A context costs one 16-byte frame allocation, and nothing global until it finds a diagnostic.

## Results once, at their size

A memo's lists are made once from an exact-size iterator (`exact_thin`, `drain(..).collect()`,
`Box::from(slice)`). That covers:
- `TyLoweringResult`'s diagnostics and constants;
- the opaques;
- `ConstParamTypes` and `GenericDefaults` (no more grow, then pad, then shrink);
- `SupertraitsInfo` (no more `extend`, then `into_boxed_slice`);
- the batch columns and the persisted batch readers (no more `collect::<Result<Box<[_]>>>`
  doubling).

An empty list allocates nothing.

## Shared lowerings return frame parts

Four lowerings serve both a per-item query and a crate's batch: impl self type, impl trait,
field types and predicates. They return `FrameLowered<'f, 'db, T>`: the value, with its findings
still in the frame. The query makes it owned (`owned()`), and the batch copies it into its side
table. A frame list cannot leave a closure whose context lifetime is higher-ranked
(`with_ty_lowering`, `with_shifted_in`), so two helpers fill the caller's list instead of
returning one: `lower_closure_params` and `ElidedLifetimes::regions_into`. `with_shifted_in`
takes `FnOnce(&mut Self)`.

## One deviation: a task per batch item

A crate's batch lowers thousands of items in one query run. With only the query's frame, every
item's temporaries pile up: about 200 bytes each, 2 MB for std's impls, written once and cold.
Each item therefore runs in a nested `frame::task(INFER_TAG)`, about 10 ns against microseconds
of lowering, so the batch peaks at its largest item's temporaries, in warm memory. R1 says a
frame is not opened deeper, and this is the one exception, told to mem-db-infer.

## Smaller wins

- `variance`: the fixpoint's solution is a frame slice of one byte per parameter, with no spill
  past 16. The value kept between iterations is the interned `StoredVariancesOf`, so it is
  already owned (contract 1).
- `dyn_compatibility`: the supertraits are read from the `SupertraitsInfo` memo (the interner's
  `supertrait_def_ids` already is that). A differential test holds it to `elaborate`'s walk. The
  `impl Trait` check stops at the first return-position opaque, in place of collecting a set.
- `variance::unused::adt_unused_params`, an entry point outside queries, opens a task and uses
  frame sets.

## Speed of light

What cannot go is one allocation per non-empty list a memo owns. Interning is the interner's.
Every other byte of lowering is in a frame, at about 2 ns an allocation against 15 to 25 ns for
the general allocator.

A 1,000-expression body's lowering makes about 400 frame allocations (8 KB) and no global ones.
Lowering a path costs 50 to 200 ns (resolution, generics, interning), so the allocator is under 2%
of lowering. Signature queries drop from about 4 global allocations of temporaries per item to
none, plus about 1.2 exact allocations per item for the memos themselves. On a crate of 10,000
items that is 0.8 ms of allocator time down to about 0.3 ms.
