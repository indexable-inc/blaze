# mem-db-infer-eval: requests

Part of `mem-db-infer` (`docs/v2/plan/lanes/mem/db-infer.md`, rules R1 to R8). Files, all in
`crates/hir-ty/src/`: `layout.rs`, `layout/adt.rs`, `layout/target.rs`, `consteval.rs`, `mir.rs`,
`mir/held.rs`, `mir/target.rs`, `intrinsic.rs`, `target_feature.rs`, `target_feature/tests.rs`,
`lang_items.rs`, `primitive.rs`, `solver_errors.rs`, `evict.rs`. Their owner is lane db (`layout.rs`
and `layout/` are lane items' too); none is done, so every change below is a request. Lines are
as of 2026-10-29 (UTC-7). Other agents are editing these files, so each entry quotes its "before"
code to find it by. The `db` parameters (`&'db Db` now, `&dyn HirDatabase` before) stay exactly
as they are wherever they appear; no entry depends on which it is. Lane dedyn's agents may edit
these files to replace hot `dyn` with concrete types, enums or generics. Where one of their edits
already does what an entry asks (L6, C2), that entry is done; where it changes a "before" quote,
apply the entry to their text and keep their change. No entry here adds a `dyn`. The same holds
for lane frontend-tree's moves of `SyntaxNode` callers to `syntax::view`. None of these files
holds a syntax handle; `lang_items.rs` reads only the `syntax::ast::BinaryOp` enum. No entry here
walks syntax.

## What these files allocate, in short

| temporary | where, how often | before | after |
|---|---|---|---|
| `held`'s bit sets (`live_in`, `live_out`, forward `borrowed`/`requires`, scratch) | `mir/held.rs`, per block of an `async` body, 3 to 4 times per body | one `DenseBitSet` (a global `Vec<u64>`) per set: about `4B + 7` blocks | `Set<'f>(&'f mut [u64])` in the frame: 0 global |
| `held`'s `reachable`, worklists, `edges`, `asm_outputs` | per block | `vec![false; B]`, `vec![start]` (grows), `SmallVec<[Edge; 3]>`, `SmallVec<[&Place; 2]>` | frame slices and `Vec`s; `edges` and `asm_outputs` call back, building nothing |
| `held`'s conflict matrix and `together` rows | per `async` body | `n` `DenseBitSet`s + `Vec<bool>` of `n²` | frame rows; the result is bit rows, `n · ⌈n/64⌉` words, made once |
| `HeldLocals` (the result) | per `async` body | `Vec`s grown by `collect` from a `filter` | `Box<[T]>`, each made once at its size |
| `univariant`'s field layouts and references | `layout.rs`, per tuple/closure layout | two `SmallVec<[_; 8]>`, both spill past 8 fields | frame `Vec` + frame slice |
| the coroutine layout's upvars, locals, `kept`, `prefix` | `layout.rs`, per coroutine layout | 1 `SmallVec` + 1 `Vec` + 2 `SmallVec` + `IndexVec` per variant | frame `Vec`s/slices; `prefix` one exact `IndexVec` (taken by value); `kept` frame slices after R-abi |
| the `Or` pattern type's ranges | `layout.rs`, per such layout (signed `NonZero`) | `Vec` from `collect` | frame `Vec` at its size, sorted in place |
| `layout_of_scalable_vector_ty`'s field types | per scalable vector layout | 2 global `Vec<Ty>` | 2 frame `Vec`s |
| `layout_of_adt`'s flat field buffer and ends | `layout/adt.rs`, per ADT layout | two `SmallVec<[_; 8]>`, spill on any enum with more than 8 fields or variants | frame `Vec`s, `ends` at its size, `fields` growing in place |
| `layout_of_adt`'s per-variant `IndexVec`s | per ADT layout | `1 + V` global (`variants_of`) | frame slices (after R-abi); until then unchanged |
| `TerminatorKind::successors` | `mir.rs`, per block per CFG walk (about ten walks per body across hir-ty-mir, borrowck, codegen, diagnostics) | `SmallVec<[BasicBlockId; 2]>`, spills for every `SwitchInt` with 2 or more values | a borrowing iterator: 0 |
| `hash_by_key` | `mir.rs`, per `MirBody` hash | `SmallVec<[(&K, &V); 8]>`, spills past 8 upvar bindings | the worker's `u32` scratch buffer |
| `SwitchTargets::new` | `mir.rs`, per switch built | two `SmallVec`s grown one push at a time | reserved at the iterator's length: exact |
| `local_to_binding_map` | `mir.rs`, per call (codegen hashes, debuginfo, IDE) | `ArenaMap` grown by `collect` | `ArenaMap::with_capacity(locals)`: 1 block |
| `Declared::of`'s unknown names | `target_feature.rs`, per function with `#[target_feature]` | `Vec<Symbol>` grown, then shrunk by `into_boxed_slice` | frame `Vec<&Symbol>`, then one exact `Box<[Symbol]>` (none when all names are known) |
| `variant_field_ids`, `enum_variant_ids` | `layout.rs`, per variant/enum (memoized) | `collect` into `Box<[_]>` | exact `Vec::with_capacity` + `into_boxed_slice`: same one block, no shrink |

Not present in this tree, though the brief names them: the const-eval queries have no per-run
buffers of their own. `const_eval_query`, `anon_const_eval_query`, `const_eval_static_query` and
`const_eval_discriminant_variant` call `db.monomorphized_runtime_mir_body` and `db.interpret_mir`;
the evaluator (`crates/hir-ty-mir/src/mir/eval.rs`, mem-codegen-mir's) builds the result
`Allocation` once and interns it, and the queries keep the `Copy` handle `StoredAllocation`.
`intrinsic.rs`, `lang_items.rs`, `primitive.rs` and `mir/target.rs` allocate nothing (static tables
and `match`es; `mir/target.rs` is an atomic word).

> forwarded to db (from requests, pass 17; told by tmux).
> applied by db-mem-infer-b-eval (21:42): nothing to apply (summary of the entries below).

## Prerequisites

- **mem-db-infer** (`lib.rs`, `Cargo.toml`, `db.rs`): requests 1 to 3 of
  `v2/requests/mem-db-infer.md`: `blaze-mem` as a dependency, `#![feature(allocator_api)]`
  (for `Vec::new_in(frame)` / `Vec::with_capacity_in(n, frame)`), and the consts
  `crate::db::INFER_TAG` and `crate::db::MIR_TAG`.
- **mem-db-salsa**: the `mem_tag = ..` option of `#[salsa::tracked]`.
- **mem-db** (`blaze-mem`): `blaze_mem::frame::current`, programmed against as
  `pub fn current<R>(f: impl FnOnce(&Frame) -> R) -> R` (the running task's frame).
- **R-abi** (coordinator, `crates/rustc_abi`, below): only entries L4's second step and A2 wait
  for it. Everything else applies without it.

> forwarded to db (from requests, pass 17; told by tmux).
> not applied by db-mem-infer-b-eval (21:42): prerequisites, not edits. State at 21:42: mem-db-infer 1 is in (`blaze-mem` in hir-ty's `Cargo.toml`); 2 (`allocator_api`) and 3 (`MIR_TAG`) are not yet (db-mem-infer-a's), and `held.rs`/`target_feature.rs` need them to compile; `blaze_mem::frame::current` is in (`frame.rs:495`); salsa's `mem_tag` (mem-db-salsa M2) is not yet in `vendor/salsa-macros`.

## Objection (sent to mem-db-infer): frames past nesting depth 8

`crates/blaze-mem/src/frame.rs:35` keeps `DEPTH = 8` spare frames per thread, and `task`
(`frame.rs:447`) takes a spare or calls `Frame::new` (a 4 GiB `Region::reserve_huge`), giving the
frame back on exit, where a full spare list drops it (a `munmap`). With one task per query (R1,
and mem-db-salsa's per-query task), every query running deeper than 8 maps and unmaps a region on
every execution, plus the page commits of a fresh mapping. These files nest deeper than that as a
matter of course: `layout_of_ty` → `layout_of_adt` → `layout_of_ty` per level of type nesting, the
coroutine layout → `monomorphized_runtime_mir_body_for_coroutine` → MIR building → inference →
the solver, `const_eval_discriminant_variant` recursing once per implicit discriminant (an enum
of 100 implicit variants recurses 100 deep), and const eval → MIR → inference → const eval.
Proposed in `blaze-mem`: one frame per thread, with a nested task a `mark` on entry and a
`reset_to` on exit (tasks nest strictly, last in first out, so this is exact), or failing that a
much larger `DEPTH` (64). This file's entries are correct either way; only their cost depends on
it.

> forwarded to db (from requests, pass 17; told by tmux).
> applied by db-mem-infer-b-eval (21:42): nothing to apply in these files (an objection to lane mem, forwarded through mem-db request 10).

## R-abi: a request to the coordinator (`crates/rustc_abi`)

The layout calculator takes the variants as owned nested `IndexVec`s, which forces one global
allocation per variant on every ADT and coroutine layout. Borrowed slices carry the same data:

1. `crates/rustc_abi/src/layout.rs`: `layout_of_struct_or_enum`, `layout_of_union` and the
   private `layout_of_struct` and `layout_of_enum` take
   `variants: &IndexSlice<VariantIdx, V>` with `V: Deref<Target = IndexSlice<FieldIdx, F>>`
   instead of `&IndexSlice<VariantIdx, IndexVec<FieldIdx, F>>`. Every use (lines 359-360, 423,
   549, 555, 610-643, 769, 815, 823 and 936 as of today) indexes, iterates or calls `len` on a
   variant, which `Deref` reaches unchanged.
2. `crates/rustc_abi/src/layout/coroutine.rs`: `layout_of_coroutine`, `coroutine::layout` and
   `coroutine_saved_local_eligibility` take `variant_fields: &IndexSlice<VariantIdx, K>` with
   `K: Deref<Target = IndexSlice<FieldIdx, LocalIdx>>`; line 58's `for local in fields` becomes
   `for local in fields.iter()`.

`IndexVec` meets both bounds, so every existing caller compiles unchanged; the layouts are the
same values. Its test: `layout/adt.rs`' `one_flat_buffer_matches_a_buffer_per_variant` (A2) runs
both shapes through the calculator and compares.

> forwarded to db (from requests, pass 17; told by tmux).
> not applied by db-mem-infer-b-eval (21:42): `crates/rustc_abi` is the coordinator's; filed in `v2/requests/db.md` under "mem-db-infer-eval, for the coordinator: `crates/rustc_abi` takes borrowed variants".

## `layout.rs`

> forwarded to db (from requests, pass 17; told by tmux).
> not applied by db-mem-infer-b-eval (21:42): lane items' file (applied by requests-items-mem, below); what is left (L4 step 2, A2) is filed in `v2/requests/db.md` under "mem-db-infer-eval, for lane items".

### L1. `layout_of_ty_query` runs in the query's frame (R1, R6)

Line 377. Before:

```rust
#[salsa::tracked(cycle_result = layout_of_ty_cycle_result, returns(clone))]
pub fn layout_of_ty_query(
    db: &Db,
    ty: StoredTy,
    trait_env: StoredParamEnvAndCrate,
) -> Result<Interned<'static, InternedLayout>, LayoutError> {
    let krate = trait_env.krate;
```

After: the tracked fn opens the frame and the body moves, otherwise unchanged except for
entries L2 to L5, into `layout_of_ty_in`:

```rust
#[salsa::tracked(
    cycle_result = layout_of_ty_cycle_result,
    returns(clone),
    mem_tag = crate::db::INFER_TAG
)]
pub fn layout_of_ty_query(
    db: &Db,
    ty: StoredTy,
    trait_env: StoredParamEnvAndCrate,
) -> Result<Interned<'static, InternedLayout>, LayoutError> {
    blaze_mem::frame::current(|frame| layout_of_ty_in(db, ty, trait_env, frame))
}

fn layout_of_ty_in(
    db: &Db,
    ty: StoredTy,
    trait_env: StoredParamEnvAndCrate,
    frame: &Frame,
) -> Result<Interned<'static, InternedLayout>, LayoutError> {
    let krate = trait_env.krate;
    // ... the body as it is, with L2 to L5 ...
}
```

with `use blaze_mem::Frame;` at the top of the file. The result is the interned layout, made once
by `intern_layout` at the end (one node in the hash-cons arena, none when an equal layout is
already interned); `returns(clone)` clones a `Copy` handle.

> forwarded to db (from requests, pass 17; told by tmux).
> applied by requests-items-mem: `layout_of_ty_query` (tag `mem_tag = Infer`) opens `frame::current` and its body is `layout_of_ty_in(db, ty, trait_env, frame)`.

### L2. `univariant` and `univariant_of` lay out from the frame

Lines 345-375. Before:

```rust
/// Fields a layout keeps on the stack before its buffers spill to the heap: each buffer is
/// eight pointers, 64 bytes.
const FIELDS_INLINE: usize = 8;

/// A tuple's or closure's fields laid out as one variant.
fn univariant<'db>(
    cx: &LayoutCx<'_>,
    db: &'db Db,
    tys: impl Iterator<Item = Ty<'db>>,
    trait_env: &StoredParamEnvAndCrate,
    kind: StructKind,
) -> Result<Layout, LayoutError> {
    let fields = tys
        .map(|ty| field_layout(db, ty.store(), trait_env.clone()))
        .collect::<Result<SmallVec<[&Layout; FIELDS_INLINE]>, _>>()?;
    univariant_of(cx, &fields, kind)
}

fn univariant_of(
    cx: &LayoutCx<'_>,
    fields: &[&Layout],
    kind: StructKind,
) -> Result<Layout, LayoutError> {
    let fields: SmallVec<[&&Layout; FIELDS_INLINE]> = fields.iter().collect();
    Ok(cx.calc.layout_of_univariant(
        IndexSlice::<RustcFieldIdx, _>::from_raw(&fields),
        &ReprOptions::default(),
        kind,
    )?)
}
```

After (`FIELDS_INLINE` goes; `layout/adt.rs` stops importing it in A1):

```rust
/// A tuple's or closure's fields laid out as one variant.
fn univariant<'db>(
    cx: &LayoutCx<'_>,
    db: &'db Db,
    frame: &Frame,
    tys: impl Iterator<Item = Ty<'db>>,
    trait_env: &StoredParamEnvAndCrate,
    kind: StructKind,
) -> Result<Layout, LayoutError> {
    let mut fields = Vec::with_capacity_in(tys.size_hint().0, frame);
    for ty in tys {
        fields.push(field_layout(db, ty.store(), trait_env.clone())?);
    }
    univariant_of(cx, frame, &fields, kind)
}

fn univariant_of(
    cx: &LayoutCx<'_>,
    frame: &Frame,
    fields: &[&Layout],
    kind: StructKind,
) -> Result<Layout, LayoutError> {
    let fields: &[&&Layout] = frame.alloc_slice_from_iter(fields.iter());
    Ok(cx.calc.layout_of_univariant(
        IndexSlice::<RustcFieldIdx, _>::from_raw(fields),
        &ReprOptions::default(),
        kind,
    )?)
}
```

Its two callers in `layout_of_ty_in` pass the frame: line 483
`univariant(&cx, db, frame, tys.iter(), &trait_env, kind)?` and line 556
`univariant(&cx, db, frame, upvars.iter(), &trait_env, StructKind::AlwaysSized)?`. `Tys::iter` is
`Copied<slice::Iter>`, so the size hint is exact and the frame `Vec` never grows. The `?` inside
the loop keeps the first error, as the `collect` did.

Test, line 996, inside `univariant_on_the_stack_matches_the_vecs`. Before:

```rust
            for &kind in kinds {
                assert_eq!(
                    univariant_of(&cx, &fields, kind),
                    univariant_with_vecs(&cx, &fields, kind),
                    "case {case}, {len} fields",
                );
            }
```

After:

```rust
            for &kind in kinds {
                assert_eq!(
                    blaze_mem::frame::task(blaze_mem::Tag::Infer, |frame| {
                        univariant_of(&cx, frame, &fields, kind)
                    }),
                    univariant_with_vecs(&cx, &fields, kind),
                    "case {case}, {len} fields",
                );
            }
```

and its comment at line 981 `// Up to twelve fields, past the eight kept on the stack.` becomes
`// Up to twelve fields.`

> forwarded to db (from requests, pass 17; told by tmux).
> applied by requests-items-mem: `univariant`/`univariant_of` take the frame (frame `Vec` at the size hint, frame slice of references), both callers pass it, `FIELDS_INLINE` is gone; the test runs `univariant_of` in `frame::task(Tag::Infer, ..)` and its comment reads "Up to twelve fields."

### L3. `layout_of_scalable_vector_ty`'s field types in the frame

Lines 250-263. Before:

```rust
fn layout_of_scalable_vector_ty<'db>(
    db: &'db Db,
    id: StructId,
    args: &GenericArgs<'db>,
    env: ParamEnvAndCrate<'db>,
    dl: &TargetDataLayout,
) -> Result<Interned<'static, InternedLayout>, LayoutError> {
    let interner = DbInterner::new_no_crate(db);
    let only_field = |id: StructId| -> Result<Vec<Ty<'db>>, LayoutError> {
        let fields = db.field_types(id.into());
        Ok(fields.iter().map(|f| (*f.1).ty().instantiate(interner, args).skip_norm_wip()).collect())
    };
```

After (the rest of the function is unchanged; `fields.first()`, `fields.len()` and
`&only_field(vector)?[..]` read a frame `Vec` the same way):

```rust
fn layout_of_scalable_vector_ty<'db>(
    db: &'db Db,
    frame: &Frame,
    id: StructId,
    args: &GenericArgs<'db>,
    env: ParamEnvAndCrate<'db>,
    dl: &TargetDataLayout,
) -> Result<Interned<'static, InternedLayout>, LayoutError> {
    let interner = DbInterner::new_no_crate(db);
    let only_field = |id: StructId| -> Result<Vec<Ty<'db>, &Frame>, LayoutError> {
        let fields = db.field_types(id.into());
        let mut tys = Vec::with_capacity_in(fields.len(), frame);
        tys.extend(fields.iter().map(|f| (*f.1).ty().instantiate(interner, args).skip_norm_wip()));
        Ok(tys)
    };
```

Its caller, line 397: `return layout_of_scalable_vector_ty(db, frame, s, &args, trait_env.as_ref(db), target);`
(argument order as it is now, with `frame` second). `FieldTypes::len` and its exact-size `iter`
(`lower.rs:2530, 2545`) make the frame `Vec` exactly as long as the fields.

> forwarded to db (from requests, pass 17; told by tmux).
> applied by requests-items-mem: `layout_of_scalable_vector_ty<'db, 'f>(db, frame, ..)`, `only_field` returns `Vec<Ty, &'f Frame>` at `fields.len()`; the caller passes `frame` second. (A named `'f` instead of the elided `&Frame` in the closure's return type, which a closure signature does not tie to the parameter.)

### L4. The coroutine branch builds its inputs in the frame

Lines 578-615. Before:

```rust
            .map_err(|_| LayoutError::Unknown)?;
            let held = body.locals_held_across_yields();
            let upvars = rustc_type_ir::inherent::Ty::tuple_fields(args.tupled_upvars_ty())
                .iter()
                .map(|ty| field_layout(db, ty.store(), trait_env.clone()))
                .collect::<Result<SmallVec<[_; FIELDS_INLINE]>, _>>()?;
            // Each local is a `MaybeUninit` of its type, as rustc has it: nothing of it is a
            // niche.
            let locals = held
                .locals
                .iter()
                .map(|&local| {
                    let of = field_layout(db, body.locals[local].ty.clone(), trait_env.clone())?;
                    let mut layout = LayoutData::unit(dl, true);
                    layout.size = of.size;
                    layout.align = of.align;
                    Ok(layout)
                })
                .collect::<Result<Vec<_>, LayoutError>>()?;
            let prefix = upvars.iter().collect::<IndexVec<RustcFieldIdx, _>>();
            let locals = locals.iter().collect::<SmallVec<[&Layout; FIELDS_INLINE]>>();
            let locals = locals.iter().collect::<SmallVec<[&&Layout; FIELDS_INLINE]>>();
            let locals = IndexSlice::<usize, _>::from_raw(&locals);
            // The body has not started, has returned or has panicked, and holds nothing then.
            let mut kept: IndexVec<RustcEnumVariantIdx, IndexVec<RustcFieldIdx, usize>> =
                IndexVec::from_elem_n(IndexVec::new(), CoroutineFrame::SUSPENDED);
            kept.extend(held.yields.iter().map(|it| it.kept.iter().copied().collect()));
            let mut conflicts = BitMatrix::new(locals.len(), locals.len());
            for a in 0..locals.len() {
                for b in 0..locals.len() {
                    if held.conflict(a, b) {
                        conflicts.insert(a, b);
                    }
                }
            }
            let state = Layout::scalar(dl, CoroutineFrame::state_scalar(held.yields.len()));
            let state = &state;
            cx.calc.layout_of_coroutine(locals, prefix, &kept, &conflicts, |_| &state)?
```

After, step 1 (applies now; needs H1's `held_locals_in` and `conflicts_of`):

```rust
            .map_err(|_| LayoutError::Unknown)?;
            let held = body.held_locals_in(frame);
            let upvar_tys = rustc_type_ir::inherent::Ty::tuple_fields(args.tupled_upvars_ty());
            let mut upvars = Vec::with_capacity_in(upvar_tys.len(), frame);
            for ty in upvar_tys.iter() {
                upvars.push(field_layout(db, ty.store(), trait_env.clone())?);
            }
            // Each local is a `MaybeUninit` of its type, as rustc has it: nothing of it is a
            // niche.
            let mut locals = Vec::with_capacity_in(held.locals.len(), frame);
            for &local in held.locals.iter() {
                let of = field_layout(db, body.locals[local].ty.clone(), trait_env.clone())?;
                let mut layout = LayoutData::unit(dl, true);
                layout.size = of.size;
                layout.align = of.align;
                locals.push(layout);
            }
            // alloc: `layout_of_coroutine` takes the prefix by value and pushes the state and the
            // promoted locals onto it; this is its one block, at the most it can hold.
            let mut prefix =
                IndexVec::<RustcFieldIdx, _>::with_capacity(upvars.len() + 1 + locals.len());
            prefix.extend(upvars.iter());
            let locals: &[&Layout] = frame.alloc_slice_from_iter(locals.iter());
            let locals: &[&&Layout] = frame.alloc_slice_from_iter(locals.iter());
            let locals = IndexSlice::<usize, _>::from_raw(locals);
            // The body has not started, has returned or has panicked, and holds nothing then.
            // alloc: `layout_of_coroutine` takes `IndexVec`s (R-abi lifts this)
            let mut kept: IndexVec<RustcEnumVariantIdx, IndexVec<RustcFieldIdx, usize>> =
                IndexVec::with_capacity(CoroutineFrame::SUSPENDED + held.yields.len());
            kept.extend((0..CoroutineFrame::SUSPENDED).map(|_| IndexVec::new()));
            kept.extend(held.yields.iter().map(|it| it.kept.iter().copied().collect()));
            // alloc: `layout_of_coroutine` reads the conflicts as a `BitMatrix`
            let mut conflicts = BitMatrix::new(locals.len(), locals.len());
            for a in 0..locals.len() {
                for b in held.conflicts_of(a) {
                    conflicts.insert(a, b);
                }
            }
            let state = Layout::scalar(dl, CoroutineFrame::state_scalar(held.yields.len()));
            let state = &state;
            cx.calc.layout_of_coroutine(locals, prefix, &kept, &conflicts, |_| &state)?
```

`prefix` borrows `upvars`' frame block and the `locals` slices borrow the frame `Vec` of
`LayoutData`, which the shadowing keeps alive to the end of the arm. The per-local loop keeps the
first error, as the `collect` did. `conflicts_of` visits only set bits: `n²` calls of `conflict`
become `n · ⌈n/64⌉` word reads.

After, step 2 (once R-abi has landed), `kept` is frame slices borrowing `held`'s rows:

```rust
            // The body has not started, has returned or has panicked, and holds nothing then.
            let kept = frame.alloc_slice_fill_with(
                CoroutineFrame::SUSPENDED + held.yields.len(),
                |at| -> &IndexSlice<RustcFieldIdx, usize> {
                    IndexSlice::from_raw(match at.checked_sub(CoroutineFrame::SUSPENDED) {
                        Some(at) => &held.yields[at].kept[..],
                        None => &[],
                    })
                },
            );
            let kept = IndexSlice::<RustcEnumVariantIdx, _>::from_raw(kept);
```

and the call becomes `cx.calc.layout_of_coroutine(locals, prefix, kept, &conflicts, |_| &state)?`.

`held` is computed here, inside a query, so it uses the query's frame (`held_locals_in`), not a
task of its own.

> forwarded to db (from requests, pass 17; told by tmux).
> applied by requests-items-mem: step 1 (`held_locals_in(frame)`, upvars and locals in frame `Vec`s, `prefix` one exact `IndexVec`, locals as frame slices, `kept` at its size, `conflicts_of`).
> held by requests-items-mem: step 2 (`kept` as frame slices) waits on R-abi (`crates/rustc_abi` still takes `&IndexSlice<_, IndexVec<_, _>>`).

### L5. The `Or` pattern type's ranges in the frame

Lines 683-709 (signed `NonZero` reaches this: `pattern_type!(i32 is ..0 | 1..)`). Before:

```rust
                PatternKind::Or(variants) => match variants[0].kind() {
                    PatternKind::Range { .. } => {
                        if let BackendRepr::Scalar(scalar) = &mut layout.backend_repr {
                            let variants: Result<Vec<_>, _> = variants
                                .iter()
                                .map(|pat| match pat.kind() {
                                    PatternKind::Range { start, end } => Ok::<_, LayoutError>((
                                        extract_const_value(start)?
                                            .try_to_bits(db, trait_env.as_ref(db))
                                            .ok_or(LayoutError::Unknown)?,
                                        extract_const_value(end)?
                                            .try_to_bits(db, trait_env.as_ref(db))
                                            .ok_or(LayoutError::Unknown)?,
                                    )),
                                    PatternKind::NotNull | PatternKind::Or(_) => {
                                        Err(LayoutError::Unknown)
                                    }
                                })
                                .collect();
                            let mut variants = variants?;
                            if !scalar.is_signed() {
                                return Err(LayoutError::HasErrorType);
                            }
                            variants.sort();
```

After:

```rust
                PatternKind::Or(variants) => match variants[0].kind() {
                    PatternKind::Range { .. } => {
                        if let BackendRepr::Scalar(scalar) = &mut layout.backend_repr {
                            let mut ranges = Vec::with_capacity_in(variants.len(), frame);
                            for pat in variants.iter() {
                                let PatternKind::Range { start, end } = pat.kind() else {
                                    return Err(LayoutError::Unknown);
                                };
                                ranges.push((
                                    extract_const_value(start)?
                                        .try_to_bits(db, trait_env.as_ref(db))
                                        .ok_or(LayoutError::Unknown)?,
                                    extract_const_value(end)?
                                        .try_to_bits(db, trait_env.as_ref(db))
                                        .ok_or(LayoutError::Unknown)?,
                                ));
                            }
                            let mut variants = ranges;
                            if !scalar.is_signed() {
                                return Err(LayoutError::HasErrorType);
                            }
                            variants.sort_unstable();
```

The rest of the arm (`variants.len()`, `variants[0]`, `variants[1]`) reads the frame `Vec` as it
read the global one. The errors come in the same order (the first failing pattern, its start
before its end). `sort_unstable` gives the same order as `sort` on `(u128, u128)` (a total order,
where equal elements are identical) and allocates nothing, where `sort` may allocate a buffer.

> forwarded to db (from requests, pass 17; told by tmux).
> applied by requests-items-mem: the ranges in a frame `Vec` at `variants.len()`, let-else on `Range`, `sort_unstable`.

### L6. `struct_tail_erasing_lifetimes` takes `impl Fn`

Lines 803-807. Before:

```rust
pub fn struct_tail_erasing_lifetimes<'a>(
    db: &'a Db,
    pointee: Ty<'a>,
    normalize: &dyn Fn(Ty<'a>) -> Ty<'a>,
) -> Ty<'a> {
```

After:

```rust
pub fn struct_tail_erasing_lifetimes<'a>(
    db: &'a Db,
    pointee: Ty<'a>,
    normalize: &impl Fn(Ty<'a>) -> Ty<'a>,
) -> Ty<'a> {
```

No allocation either way; this removes an indirect call per struct level on the hot pointer
layout path. The recursion passes `normalize` on unchanged. Callers compile unchanged (each
passes `&closure`): `layout.rs:513`, `crates/hir-ty-borrowck/src/check.rs:1051`,
`crates/hir-ty-codegen/debuginfo/types.rs:509`.

> forwarded to db (from requests, pass 17; told by tmux).
> applied already (seen by requests-items-mem): `struct_tail_erasing_lifetimes<'a, F: Fn(Ty<'a>) -> Ty<'a>>(.., normalize: &F)` is in the tree; nothing to change.

### L7. `variant_field_ids` and `enum_variant_ids` at exact size (R4, R6)

Lines 193-204. Before:

```rust
#[salsa::tracked(returns(deref))]
pub fn variant_field_ids(db: &Db, variant: VariantId) -> Box<[LocalFieldId]> {
    variant.fields(db).fields().iter().map(|(id, _)| id).collect()
}

/// The variants of an enum, in order: what a layout reads of them besides their fields.
#[salsa::tracked(returns(deref))]
pub fn enum_variant_ids(db: &Db, id: EnumId) -> Box<[EnumVariantId]> {
    id.enum_variants(db).variants.values().map(|&(variant, _)| variant).collect()
}
```

After:

```rust
#[salsa::tracked(returns(deref), mem_tag = crate::db::INFER_TAG)]
pub fn variant_field_ids(db: &Db, variant: VariantId) -> Box<[LocalFieldId]> {
    let fields = variant.fields(db).fields();
    let mut ids = Vec::with_capacity(fields.len());
    ids.extend(fields.iter().map(|(id, _)| id));
    ids.into_boxed_slice()
}

/// The variants of an enum, in order: what a layout reads of them besides their fields.
#[salsa::tracked(returns(deref), mem_tag = crate::db::INFER_TAG)]
pub fn enum_variant_ids(db: &Db, id: EnumId) -> Box<[EnumVariantId]> {
    let variants = &id.enum_variants(db).variants;
    let mut ids = Vec::with_capacity(variants.len());
    ids.extend(variants.values().map(|&(variant, _)| variant));
    ids.into_boxed_slice()
}
```

One block at its size, none for an empty variant; the `collect` this replaces goes through an
iterator whose length the standard library can't trust, so it could allocate and then shrink.
The memo is the `Box<[_]>`; it holds `LocalFieldId`s/`EnumVariantId`s only.

> forwarded to db (from requests, pass 17; told by tmux).
> applied by requests-items-mem: both build their `Box<[_]>` from an exact `Vec::with_capacity`, tag `mem_tag = Infer`.

### L8. Tags of the other tracked fns (R6)

`struct_flags` (line 174) and `struct_tail_may_be_unsized` (line 183) get
`#[salsa::tracked(mem_tag = crate::db::INFER_TAG)]`; nothing else changes. `struct_flags`' memo is
a `Copy` `StructFlags`. `struct_tail_may_be_unsized`'s is a `bool`; it builds an `InferCtxt`, whose
allocations are the solver's (mem-db-solver's), so it needs no frame of its own beyond the task's.

> forwarded to db (from requests, pass 17; told by tmux).
> applied by requests-items-mem: `struct_flags` and `struct_tail_may_be_unsized` take `mem_tag = Infer`.

### L9. Imports

Drop `use smallvec::SmallVec;` (line 26) once L2 and L4 are in; the tests' `SmallVec` use (none
today) would need its own import. Add `use blaze_mem::Frame;`.

> forwarded to db (from requests, pass 17; told by tmux).
> applied by requests-items-mem: `use smallvec::SmallVec;` dropped, `use blaze_mem::Frame;` added.

## `layout/adt.rs`

> forwarded to db (from requests, pass 17; told by tmux).
> not applied by db-mem-infer-b-eval (21:42): lane items' file, as `layout.rs` above.

### A1. `layout_of_adt_query` in the query's frame (R1, R6)

Lines 27-51. Before:

```rust
#[salsa::tracked(cycle_result = layout_of_adt_cycle_result, returns(clone))]
pub fn layout_of_adt_query(
    db: &Db,
    def: AdtId,
    args: StoredGenericArgs,
    trait_env: StoredParamEnvAndCrate,
) -> Result<Interned<'static, InternedLayout>, LayoutError> {
    let krate = trait_env.krate;
    ...
    let args = args.as_ref();
    // Every variant's field layouts in one buffer, variant `i` at `fields[ends[i - 1]..ends[i]]`.
    let mut fields = SmallVec::<[&Layout; FIELDS_INLINE]>::new();
    let mut ends = SmallVec::<[u32; FIELDS_INLINE]>::new();
    let mut push_variant = |variant: VariantId| -> Result<(), LayoutError> {
```

After:

```rust
#[salsa::tracked(
    cycle_result = layout_of_adt_cycle_result,
    returns(clone),
    mem_tag = crate::db::INFER_TAG
)]
pub fn layout_of_adt_query(
    db: &Db,
    def: AdtId,
    args: StoredGenericArgs,
    trait_env: StoredParamEnvAndCrate,
) -> Result<Interned<'static, InternedLayout>, LayoutError> {
    blaze_mem::frame::current(|frame| layout_of_adt_in(db, def, args, trait_env, frame))
}

fn layout_of_adt_in(
    db: &Db,
    def: AdtId,
    args: StoredGenericArgs,
    trait_env: StoredParamEnvAndCrate,
    frame: &Frame,
) -> Result<Interned<'static, InternedLayout>, LayoutError> {
    let krate = trait_env.krate;
    ...
    let args = args.as_ref();
    let variant_count = match def {
        AdtId::StructId(_) | AdtId::UnionId(_) => 1,
        AdtId::EnumId(e) => enum_variant_ids(db, e).len(),
    };
    // Every variant's field layouts in one buffer, variant `i` at `fields[ends[i - 1]..ends[i]]`.
    // `ends` is made at its size first, so that `fields`, the frame's last block, grows in place.
    let mut ends = Vec::with_capacity_in(variant_count, frame);
    let mut fields = Vec::new_in(frame);
    let mut push_variant = |variant: VariantId| -> Result<(), LayoutError> {
```

The rest of the body is unchanged: `push_variant` pushes as before, `variants_of(&fields, &ends)`
derefs both to slices. `enum_variant_ids` is a memo hit (the enum arm asks it again). The field
layouts' queries run in tasks of their own, so no allocation of theirs lands on this frame between
`fields`' pushes.

`variant_field_layouts` (lines 131-147) takes the frame `Vec`. Before:

```rust
    trait_env: &StoredParamEnvAndCrate,
    out: &mut SmallVec<[&'static Layout; FIELDS_INLINE]>,
) -> Result<(), LayoutError> {
```

After:

```rust
    trait_env: &StoredParamEnvAndCrate,
    out: &mut Vec<&'static Layout, &Frame>,
) -> Result<(), LayoutError> {
```

Imports: drop `use smallvec::SmallVec;` and `FIELDS_INLINE` from the `crate::layout` import; add
`use blaze_mem::Frame;`. `const_eval_discriminant` per variant (line 101) is a memo hit after the
first layout of the enum.

Test, lines 230-253 (`nested_with_vecs` and `flat`), which use `SmallVec` and `FIELDS_INLINE`
through `use super::*`. Before:

```rust
        let variants = variants
            .iter()
            .map(|it| it.to_vec())
            .collect::<SmallVec<[_; 1]>>();
```
```rust
        let mut fields = SmallVec::<[&Layout; FIELDS_INLINE]>::new();
        let mut ends = SmallVec::<[u32; FIELDS_INLINE]>::new();
        for variant in variants {
            fields.extend(variant.iter().copied());
            ends.push(fields.len() as u32);
        }
        layout(&variants_of(&fields, &ends))
```

After:

```rust
        let variants = variants.iter().map(|it| it.to_vec()).collect::<Vec<_>>();
```
```rust
        blaze_mem::frame::task(blaze_mem::Tag::Infer, |frame| {
            let mut ends = Vec::with_capacity_in(variants.len(), frame);
            let mut fields = Vec::new_in(frame);
            for variant in variants {
                fields.extend(variant.iter().copied());
                ends.push(fields.len() as u32);
            }
            layout(&variants_of(&fields, &ends))
        })
```

> forwarded to db (from requests, pass 17; told by tmux).
> applied by requests-items-mem: `layout_of_adt_query` (tag `Infer`) opens `frame::current` into `layout_of_adt_in(.., frame)`; `ends` at the variant count and `fields` growing last in the frame; `variant_field_layouts` takes `&mut Vec<&'static Layout, &Frame>`; `SmallVec`/`FIELDS_INLINE` imports gone; the tests' `nested_with_vecs` collects a `Vec` and `flat` runs in `frame::task(Tag::Infer, ..)`.

### A2. `variants_of` in the frame (after R-abi)

Lines 149-164. Before:

```rust
/// The fields of each variant as the layout calculator takes them: variant `i` is
/// `refs[ends[i - 1]..ends[i]]`. The calculator wants an owned `IndexVec` per variant; each is
/// one exact allocation, none for a variant without fields.
fn variants_of<'a, 'b>(
    refs: &'a [&'b Layout],
    ends: &[u32],
) -> IndexVec<RustcEnumVariantIdx, IndexVec<RustcFieldIdx, &'a &'b Layout>> {
    let mut variants = IndexVec::with_capacity(ends.len());
    let mut start = 0;
    for &end in ends {
        let end = end as usize;
        variants.push(refs[start..end].iter().collect());
        start = end;
    }
    variants
}
```

After:

```rust
/// The fields of each variant as the layout calculator takes them: variant `i` is
/// `fields[ends[i - 1]..ends[i]]`, every slice in the frame.
fn variants_of<'a, 'b>(
    frame: &'a Frame,
    fields: &'a [&'b Layout],
    ends: &[u32],
) -> &'a IndexSlice<RustcEnumVariantIdx, &'a IndexSlice<RustcFieldIdx, &'a &'b Layout>> {
    let refs: &'a [&'a &'b Layout] = frame.alloc_slice_from_iter(fields.iter());
    let mut start = 0;
    let variants = frame.alloc_slice_from_iter(ends.iter().map(|&end| {
        let end = end as usize;
        let variant = IndexSlice::<RustcFieldIdx, _>::from_raw(&refs[start..end]);
        start = end;
        variant
    }));
    IndexSlice::from_raw(variants)
}
```

The query's call becomes `let variants = variants_of(frame, &fields, &ends);`, and the two calls
pass `variants` where they passed `&variants` (`layout_of_union(&repr, variants)`,
`layout_of_struct_or_enum(&repr, variants, ..)`). `use rustc_index::IndexVec;` becomes
`use rustc_index::IndexSlice;`.

The test's closures over the concrete `Variants` type can't take both shapes, so they become one
generic helper. In `one_flat_buffer_matches_a_buffer_per_variant` (lines 255-306) the two closures
`struct_or_enum` and `union` and the type alias `Variants` are replaced by:

```rust
    fn struct_or_enum<'a, 'b: 'a, V>(
        cx: &LayoutCx<'_>,
        dl: &TargetDataLayout,
        variants: &IndexSlice<RustcEnumVariantIdx, V>,
        is_enum: bool,
        unsized_tail: bool,
    ) -> Result<Layout, LayoutError>
    where
        V: std::ops::Deref<Target = IndexSlice<RustcFieldIdx, &'a &'b Layout>>,
    {
        let repr = ReprOptions::default();
        Ok(cx.calc.layout_of_struct_or_enum(
            &repr,
            variants,
            is_enum,
            NicheOptimizations::Enabled,
            |min, max| repr_discr(dl, &repr, min.start, max.last).unwrap_or((Integer::I8, false)),
            (0..variants.len()).map(|it| (RustcEnumVariantIdx(it), it as u128)),
            !is_enum && !unsized_tail,
        )?)
    }
```

and the same shape for `union` over `cx.calc.layout_of_union(&repr, variants)`. The nested side
builds `IndexVec<_, IndexVec<_, _>>` as `nested_with_vecs` does now and calls the helper on it;
the flat side, inside `frame::task(Tag::Infer, ..)`, calls it on `variants_of(frame, &fields,
&ends)`. The assertion compares the two `Result<Layout, _>`s as now.

> forwarded to db (from requests, pass 17; told by tmux).
> held by requests-items-mem: waits on R-abi (`crates/rustc_abi` `layout_of_struct_or_enum`/`layout_of_union` still take nested `IndexVec`s); `variants_of`'s per-variant `collect` carries an `// alloc:` note naming it.
> forwarded by db-audit-mem (22:44) to lane items (requests-items-mem): `crates/hir-ty/src/layout/adt.rs` is lane items', filed in `v2/requests/db.md` under "mem-db-infer-eval, for lane items"; it waits on R-abi (filed there for the coordinator), not landed: `crates/rustc_abi/src/layout.rs:346` and `:400` still take `&IndexSlice<VariantIdx, IndexVec<FieldIdx, F>>`.

## `layout/target.rs`

> forwarded to db (from requests, pass 17; told by tmux).
> not applied by db-mem-infer-b-eval (21:43): lane items' file; T1 applied by requests-items-mem.

### T1. Tag only (R6)

Line 8: `#[salsa::tracked(returns(as_ref))]` becomes
`#[salsa::tracked(returns(as_ref), mem_tag = crate::db::INFER_TAG)]`. The memo is the owned
`TargetDataLayout`, parsed once per crate. Its `format!` and `e.clone()` are the error path (a
target spec that doesn't parse); left as they are.

> forwarded to db (from requests, pass 17; told by tmux).
> applied by requests-items-mem: `#[salsa::tracked(returns(as_ref), mem_tag = Infer)]`.

## `consteval.rs`

> forwarded to db (from requests, pass 17; told by tmux).
> applied by db-mem-infer-b-eval (21:43): C1 to C3, below.

### C1. Tags (R6)

| line | fn | tag | memo |
|---|---|---|---|
| 431 | `const_eval_discriminant_variant` | `INFER_TAG` | `Result<i128, ConstEvalError>`, `returns(clone)`: `Ok` is a number |
| 503 | `const_eval_query` | `INFER_TAG` | `Result<StoredAllocation, ConstEvalError>`, `returns(ref)`, `lru = 0`: `Ok` is a `Copy` handle to the interned `Allocation` |
| 548 | `anon_const_eval_query` | `INFER_TAG` | as above |
| 588 | `const_eval_static_query` | `INFER_TAG` | as above |

Each attribute gains `, mem_tag = crate::db::INFER_TAG`, e.g. line 503:
`#[salsa::tracked(returns(ref), cycle_result = const_eval_cycle_result, lru = 0, mem_tag = crate::db::INFER_TAG)]`.
They build no temporaries: the work is `monomorphized_runtime_mir_body` and `interpret_mir`
(mem-codegen-mir's evaluator), each a query with its own task. So they need no `current` of
their own. `const_eval_discriminant_variant` returns `Err` by clone on a hit (`returns(clone)`),
which allocates when the error holds a `String`; that is the error path. If wanted,
`returns(ref)` plus `.clone()` at its one wrapper (`db.rs`, mem-db-infer's) would make even that
a reference.

> forwarded to db (from requests, pass 17; told by tmux).
> applied by db-mem-infer-b-eval (21:39): the four attributes gain `mem_tag = Infer` (the bare variant, as salsa's option is filed in mem-db-salsa M2), not `crate::db::INFER_TAG`. `returns(clone)` on `const_eval_discriminant_variant` left as is.

### C2. `write_memory_map` and `read_memory_map` take `impl` closures

Lines 776-780 and 853-857. Before:

```rust
fn write_memory_map<'db>(
    out: &mut Writer,
    map: &MemoryMap<'db>,
    ty: &mut dyn FnMut(Ty<'db>) -> u32,
) -> MapColumns {
```
```rust
fn read_memory_map<'db>(
    record: &MemoryMapRec,
    buffer: &[u8],
    ty: &dyn Fn(u32) -> Result<Ty<'db>, FormatError>,
) -> Result<MemoryMap<'db>, FormatError> {
```

After:

```rust
fn write_memory_map<'db>(
    out: &mut Writer,
    map: &MemoryMap<'db>,
    ty: &mut impl FnMut(Ty<'db>) -> u32,
) -> MapColumns {
```
```rust
fn read_memory_map<'db>(
    record: &MemoryMapRec,
    buffer: &[u8],
    ty: &impl Fn(u32) -> Result<Ty<'db>, FormatError>,
) -> Result<MemoryMap<'db>, FormatError> {
```

The four callers (lines 682, 703, 938, 946) pass `&mut |..| ..` / `&|..| ..` and compile
unchanged; the two test closures that diverge (`unreachable!`) take their return type from the
bound. One indirect call per type in a memory map goes away; no allocation either way.

> forwarded to db (from requests, pass 17; told by tmux).
> applied (by lane dedyn, dedyn-ty, before db-mem-infer-b-eval got here; checked 21:39): both take `impl` closures as written.

### C3. Hit paths (R7) and what stays

`try_const_usize`, `evaluated` and the `const_eval` / `anon_const_eval` / `const_eval_static`
wrappers read the memo by reference and allocate nothing on `Ok`; their `Err(err.clone())` is the
error path. `trace_const` is `None` unless an observer is set. `read_allocation` /
`read_memory_map`'s `Box::from`, `Vec::with_capacity`, `bytes.to_vec()` and `Box::new(map)` build
the owned `Allocation` being read back from the image: exact, once, the result. `Consts::new_from_iter`
interns. `path_to_const` and `create_anon_const` (lines 314, 361) take `&dyn Fn() -> &Generics` and
`Option<&mut dyn FnMut(Span) -> Const>` from the lowering context (mem-db-lower's callers), once
per const argument; they allocate nothing, and making them generic would monomorphize lowering's
callers, so they stay, each with its reason on the parameter (lane dedyn's rule):

```rust
    // dyn: called once per const argument from lowering; generic would copy lowering's callers
    generics: &dyn Fn() -> &'a Generics<'db>,
```

and on `create_anon_const`'s `create_var`:

```rust
    // dyn: called once per inferred const argument; generic would copy lowering's callers
    create_var: Option<&mut dyn FnMut(Span) -> Const<'db>>,
```

If lane dedyn makes these generic or concrete first, their change stands and this entry is
done.

> forwarded to db (from requests, pass 17; told by tmux).
> applied by db-mem-infer-b-eval (21:40): lane dedyn made `path_to_const`'s and `create_anon_const`'s parameters `impl` first, so the `// dyn:` notes are moot and their change stands. What stays now carries its reason for the hot lint: the three wrappers' `Err(err.clone())` (`// clone: error path`) and `read_memory_map`'s `bytes.to_vec()` and `Box::new(map)` (`// alloc:` the owned allocation read back once). Also `intrinsic.rs`' `name.clone()` in `intrinsic_of` (`// clone: error path`, as "evict.rs, intrinsic.rs, ..." below says).

## `mir.rs`

> forwarded to db (from requests, pass 17; told by tmux).
> applied by db-mem-infer-b-eval (21:43): M1 to M4 and M6, below; M5 waits on the builder (mem-codegen-mir's).

### M1. `TerminatorKind::successors` returns a borrowing iterator

Lines 1315-1336. Before:

```rust
    /// The blocks control goes to from here when nothing unwinds, and the coroutine is not
    /// dropped where it yields.
    pub fn successors(&self) -> SmallVec<[BasicBlockId; 2]> {
        match self {
            TerminatorKind::Goto { target }
            | TerminatorKind::Drop { target, .. }
            | TerminatorKind::DropAndReplace { target, .. }
            | TerminatorKind::Assert { target, .. }
            | TerminatorKind::FalseEdge { real_target: target, .. }
            | TerminatorKind::FalseUnwind { real_target: target, .. } => smallvec![*target],
            TerminatorKind::SwitchInt { targets, .. } => targets.all_targets().into(),
            TerminatorKind::Call { target, .. } => target.iter().copied().collect(),
            TerminatorKind::Yield { resume, drop, .. } => {
                std::iter::once(*resume).chain(*drop).collect()
            }
            TerminatorKind::UnwindResume
            | TerminatorKind::Abort
            | TerminatorKind::Return
            | TerminatorKind::Unreachable
            | TerminatorKind::CoroutineDrop => smallvec![],
        }
    }
```

After:

```rust
    /// The blocks control goes to from here when nothing unwinds, and the coroutine is not
    /// dropped where it yields.
    pub fn successors(&self) -> Successors<'_> {
        let (pair, switch): ([Option<BasicBlockId>; 2], &[BasicBlockId]) = match self {
            TerminatorKind::Goto { target }
            | TerminatorKind::Drop { target, .. }
            | TerminatorKind::DropAndReplace { target, .. }
            | TerminatorKind::Assert { target, .. }
            | TerminatorKind::FalseEdge { real_target: target, .. }
            | TerminatorKind::FalseUnwind { real_target: target, .. } => ([Some(*target), None], &[]),
            TerminatorKind::SwitchInt { targets, .. } => ([None, None], targets.all_targets()),
            TerminatorKind::Call { target, .. } => ([*target, None], &[]),
            TerminatorKind::Yield { resume, drop, .. } => ([Some(*resume), *drop], &[]),
            TerminatorKind::UnwindResume
            | TerminatorKind::Abort
            | TerminatorKind::Return
            | TerminatorKind::Unreachable
            | TerminatorKind::CoroutineDrop => ([None, None], &[]),
        };
        Successors { pair, at: 0, switch: switch.iter() }
    }
```

and, next to `TerminatorKind`:

```rust
/// The blocks control goes to from a terminator, borrowed from it: what
/// [`TerminatorKind::successors`] gives, in the same order.
#[derive(Debug, Clone)]
pub struct Successors<'a> {
    /// The targets of a terminator with at most two, `None` where it has fewer.
    pair: [Option<BasicBlockId>; 2],
    at: usize,
    /// A switch's targets.
    switch: std::slice::Iter<'a, BasicBlockId>,
}

impl Iterator for Successors<'_> {
    type Item = BasicBlockId;

    #[inline]
    fn next(&mut self) -> Option<BasicBlockId> {
        while self.at < 2 {
            let it = self.pair[self.at];
            self.at += 1;
            if it.is_some() {
                return it;
            }
        }
        self.switch.next().copied()
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let len = self.pair[self.at.min(2)..].iter().flatten().count() + self.switch.len();
        (len, Some(len))
    }
}

impl ExactSizeIterator for Successors<'_> {}
```

The order is the same for every kind (a call's target, a yield's resume then its drop, a switch's
targets in `all_targets` order). Every caller iterates, extends or chains it, so they compile
unchanged: hir-ty-mir `move_paths.rs:603, 717`, `elaborate_drops.rs:160`, `storage.rs:144, 215,
225, 314`, `const_check.rs:308`; hir-ty-codegen `f16_f128.rs:539`, `fma.rs:252, 1506`;
blaze-diagnostics `liveness/flow.rs:236`, `panics.rs:741`, `recursion.rs:148`; `held.rs` (H1).
`.into_iter()` on an iterator is the iterator. One caller needs a one-line change, in a file of
mem-db-checks' (borrowck), to apply with this entry:

`crates/hir-ty-borrowck/src/cfg.rs:47`. Before:

```rust
            let mut all = kind.successors();
            all.extend(kind.unwind());
            all
```

After (the same `SmallVec` it returned, built from the iterator):

```rust
            let mut all: SmallVec<[BasicBlockId; 2]> = kind.successors().collect();
            all.extend(kind.unwind());
            all
```

(if that function's return type is spelled differently, `collect` into it; the extend stays).
`smallvec!` stays imported for `SwitchTargets::static_if`.

> forwarded to db (from requests, pass 17; told by tmux).
> applied by db-mem-infer-b-eval (21:34): `successors` and `Successors` as written. The borrowck edit is overtaken: `cfg.rs`'s own copy went with db.md's "dedup: TerminatorKind::all_successors", which put `all_successors` in `mir.rs`; that fn's non-`FalseEdge` arm now does this entry's `collect` into its `SmallVec<[BasicBlockId; 2]>` and extends with the unwind, so its callers (borrowck `cfg.rs:51`, hir-ty-mir, blaze-items) are unchanged. `// alloc:` notes on both of its arms (db-apply-ty's ask). Told db-mem-borrowck.

### M2. `hash_by_key` sorts the keys in the worker's scratch buffer

Lines 1456 and 1472-1486. Before:

```rust
        hash_by_key(upvar_locals.iter(), state);
```
```rust
/// Hashes the entries of a map (distinct keys) in the order of their keys, so that maps that
/// are `==` hash the same whatever order they iterate in. Straight into `state`, with no
/// hasher per entry; up to 8 entries sort on the stack.
fn hash_by_key<'a, K: Hash + Ord + 'a, V: Hash + 'a, H: Hasher>(
    entries: impl Iterator<Item = (&'a K, &'a V)>,
    state: &mut H,
) {
    let mut entries: SmallVec<[(&K, &V); 8]> = entries.collect();
    entries.sort_unstable_by(|a, b| a.0.cmp(b.0));
    state.write_usize(entries.len());
    for (key, value) in entries {
        key.hash(state);
        value.hash(state);
    }
}
```

After:

```rust
        hash_by_key(
            upvar_locals,
            |binding| binding.into_raw().into_u32(),
            |raw| BindingId::from_raw(RawIdx::from_u32(raw)),
            state,
        );
```
```rust
/// Hashes the entries of `map` in the order of their keys, so that maps that are `==` hash the
/// same whatever order they iterate in. The keys sort as `u32`s in the worker's scratch buffer,
/// `raw` keeping their order, and each value is found again by its key; straight into `state`,
/// with no hasher per entry.
fn hash_by_key<K: Hash + Eq + Copy, V: Hash, H: Hasher>(
    map: &FxHashMap<K, V>,
    raw: impl Fn(K) -> u32,
    from_raw: impl Fn(u32) -> K,
    state: &mut H,
) {
    blaze_mem::scratch::with_u32s(|keys| {
        keys.extend(map.keys().map(|&key| raw(key)));
        keys.sort_unstable();
        state.write_usize(keys.len());
        for &key in keys.iter() {
            let key = from_raw(key);
            key.hash(state);
            map[&key].hash(state);
        }
    })
}
```

The bytes hashed are identical: the length, then each key and value in ascending key order
(`Idx`'s order is its raw index's). `upvar_locals` is
`FxHashMap<BindingId, Vec<(LocalId, Place)>>` (line 1387); a body with more than 8 captured
bindings no longer allocates. The extra lookup per key is a hash probe, cheaper than the allocator.

Tests, lines 2338-2397. `digest` takes the map:

```rust
    fn digest(map: &FxHashMap<u32, Vec<u32>>) -> (u64, u64) {
        let mut hasher = salsa::StableHasher::new();
        hash_by_key(map, |key| key, |raw| raw, &mut hasher);
        hasher.finish128()
    }
```

Every `digest(x.iter())` becomes `digest(&x)`; `a_small_map_hashes_by_key_on_the_stack_too` is
renamed `a_small_map_hashes_by_key`; in `unordered_hash_ignores_insertion_order` the three lines
over `refs` (from `let pairs = entries();` to the second `assert_eq!` on `refs`) go, since there
is no slice form left to compare (the forward/backward map assertion above them stays).

> forwarded to db (from requests, pass 17; told by tmux).
> applied by db-mem-infer-b-eval (21:34): as written (call, fn, tests); `Hash for MirBody` now also hashes `inlined` (codegen-inline section 1), untouched.

### M3. `SwitchTargets::new` builds both lists at their size (R4)

Lines 336-343. The lists are part of the owned body. Before:

```rust
    pub fn new(
        targets: impl Iterator<Item = (u128, BasicBlockId)>,
        otherwise: BasicBlockId,
    ) -> Self {
        let (values, mut targets): (SmallVec<_>, SmallVec<_>) = targets.unzip();
        targets.push(otherwise);
        Self { values, targets }
    }
```

After:

```rust
    pub fn new(
        targets: impl Iterator<Item = (u128, BasicBlockId)>,
        otherwise: BasicBlockId,
    ) -> Self {
        let count = targets.size_hint().0;
        let mut values = SmallVec::with_capacity(count);
        let mut blocks = SmallVec::with_capacity(count + 1);
        for (value, target) in targets {
            values.push(value);
            blocks.push(target);
        }
        blocks.push(otherwise);
        Self { values, targets: blocks }
    }
```

`unzip` pushes one at a time (growing 2, 4, 8 and so on once spilled) and the `otherwise` push
grows `targets` again; with an exact hint this is one block per spilled list, none for a two-way
switch. Same values in the same order.

> forwarded to db (from requests, pass 17; told by tmux).
> applied by db-mem-infer-b-eval (21:35): as written.

### M4. `local_to_binding_map` at its size

Lines 1701-1703. Before:

```rust
    pub fn local_to_binding_map(&self) -> ArenaMap<LocalId, BindingId> {
        self.binding_locals.iter().map(|(it, y)| (*y, it)).collect()
    }
```

After:

```rust
    pub fn local_to_binding_map(&self) -> ArenaMap<LocalId, BindingId> {
        // alloc: the caller's map, one block over the body's locals
        let mut map = ArenaMap::with_capacity(self.locals.len());
        for (binding, &local) in self.binding_locals.iter() {
            map.insert(local, binding);
        }
        map
    }
```

Same inserts in the same order (a later binding of the same local wins, as before). Callers own
the map: hir-ide `mir_pretty`, `crates/hir-ty-codegen/debuginfo/types.rs:621`,
`crates/hir-ty-codegen/v2/hashes.rs:265`; all run outside queries.

> forwarded to db (from requests, pass 17; told by tmux).
> applied by db-mem-infer-b-eval (21:35): as written.

### M5. `MirBody::shrink_to_fit` and `VTableMap::shrink_to_fit` (R4): for mem-codegen-mir

`mir.rs:1756` and `mir.rs:2023` exist only for the builder: `hir-ty-mir/src/mir/lower.rs:4852,
4934, 4965`, `transform.rs:157` and `eval.rs:777` call them. R4 forbids shrinking a result after
the fact; the fix is in the builder (build the body's lists in the frame, make each owned list
once at its size), which is mem-codegen-mir's file. These methods go when nothing calls them; no
change in `mir.rs` until then.

> forwarded to db (from requests, pass 17; told by tmux).
> not applied by db-mem-infer-b-eval (21:35): nothing in `mir.rs` until the builder stops calling them; the builder side is in `v2/requests/mem-codegen-mir.md` (its `shrink_to_fit` entries, lines 1040-1048), so nothing new is filed.

### M6. Left as they are

- `spread_rust_call_tuple` (used by codegen `mod.rs:3657`) mutates an owned body once per
  `extern "rust-call"` shim; its `collect` into the `Box<[Operand]>` of the aggregate is that body's
  own list.
- `mark_moves`' `place.clone()` (line 1740): a `StoredPlace` is a local and an interned
  projection handle, no heap block.
- `Projection::project` interns; `Operand::from_concrete_const` moves the caller's `Box<[u8]>` into
  the interner (interning by a borrowed key would be mem-db-solver's).
- `Budget::setting`'s `format!`, `MirEvalError` / `MirLowerError` `String`s: the error path.
- `is_by_move_coroutine` (line 2215): reads the expression store and a memo, allocates nothing.
- `SwitchTargets`' `SmallVec<[_; 1]>` / `SmallVec<[_; 2]>` fields: the owned body's type; a
  two-way switch stays inline, and M3 makes the spilled ones exact.

> forwarded to db (from requests, pass 17; told by tmux).
> applied by db-mem-infer-b-eval (21:35): left as they are, each now with its reason above as a `// alloc:` / `// clone:` mark (`static_if`, `spread_rust_call_tuple`'s two, `mark_moves`, `Budget::setting`), so the hot lint passes `mir.rs`.

## `mir/held.rs`

> forwarded to db (from requests, pass 17; told by tmux).
> applied by db-mem-infer-b-eval (21:43): H1, below.

### H1. The analysis runs in a frame; the result is made once at its size

`locals_held_across_yields` is called from `layout.rs:579` (inside `layout_of_ty_query`, which
moves to `held_locals_in` in L4) and from outside any query: `blaze-items/src/form.rs:1692`,
`hir-ty-codegen/debuginfo/types.rs:618` and `hir-ty-codegen/mod.rs:10332`. So it is an R1 entry
point that opens a task of `MIR_TAG`, and `held_locals_in` serves callers that have a frame.

Every bit set was a `rustc_index` `DenseBitSet`, whose words are a global `Vec`: one allocation
per set, `4B + 7` per run for `B` blocks, plus the `Vec`s of `Option<Set>`, the matrix and the
result's growth. After, every set is a frame slice, and the result's lists are made once each.
Callers only read the fields (`&held.locals`, `.len()`, `.binary_search`, `.iter().position`,
`it.kept.iter()`), which a `Box<[T]>` gives as a `Vec` did.

The whole file after the change (the comments, the analyses and their order are as now; what
changes is `Set`, the frame parameters, `edges` / `asm_outputs` calling back, and the results):

```rust
//! What an `async` block keeps in its future from one `poll` to the next, computed as rustc's
//! coroutine transform computes it on the runtime body (`rustc_mir_transform/src/coroutine/
//! layout.rs` at 1.99.0: `locals_live_across_suspend_points`, `compute_storage_conflicts` and
//! the variant fields of `compute_layout`), with the analyses it runs (`MaybeBorrowedLocals`,
//! `MaybeRequiresStorage`, `MaybeLiveLocals` and `always_storage_live_locals` of
//! `rustc_mir_dataflow`). Given the same body, the same locals are kept in the same variants
//! with the same conflicts, and `rustc_abi`'s coroutine layout places them as rustc does.
//!
//! rustc's runtime body has had its `FakeRead`s removed (`CleanupPostBorrowck`); they are
//! ignored here. No statement unwinds: what panics is an `Assert` before it.

use blaze_mem::Frame;

use super::{
    AsmOperand, BasicBlockId, BorrowKind, LocalId, MirBody, Operand, OperandKind, ProjectionElem,
    Rvalue, Statement, StatementKind, StoredPlace, Terminator, TerminatorKind,
};

/// The locals an `async` block keeps in its future: rustc's `CoroutineSavedLocal`s.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldLocals {
    /// The locals live across some yield, in the order the body declares them.
    pub locals: Box<[LocalId]>,
    /// The places the body yields at, in the order of its blocks: the states of the future
    /// after the three rustc reserves (not started, returned, panicked).
    pub yields: Box<[YieldPoint]>,
    /// Whether two of the locals may need their storage at once: per local a row of
    /// `locals.len().div_ceil(64)` words, a bit per local.
    conflicts: Box<[u64]>,
}

/// One place the body yields at, which is one variant of the future.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct YieldPoint {
    pub resume: BasicBlockId,
    /// Where a future suspended here is dropped.
    pub drop: Option<BasicBlockId>,
    /// The fields of the variant: the held locals live here, by their index in
    /// [`HeldLocals::locals`].
    pub kept: Box<[usize]>,
}

impl HeldLocals {
    pub fn contains(&self, local: LocalId) -> bool {
        self.locals.binary_search(&local).is_ok()
    }

    /// Whether the `a`th and the `b`th of the locals may need their storage at once, and so
    /// can't share memory.
    pub fn conflict(&self, a: usize, b: usize) -> bool {
        let row = row(&self.conflicts, self.locals.len(), a);
        (row[b / 64] >> (b % 64)) & 1 != 0
    }

    /// The locals the `a`th may need its storage at once with, by their index in
    /// [`HeldLocals::locals`], in order.
    pub fn conflicts_of(&self, a: usize) -> impl Iterator<Item = usize> + '_ {
        bits(row(&self.conflicts, self.locals.len(), a))
    }
}

/// The `a`th row of `n` locals' conflicts.
fn row(conflicts: &[u64], n: usize, a: usize) -> &[u64] {
    let words = n.div_ceil(64);
    &conflicts[a * words..(a + 1) * words]
}

/// The positions of the set bits of `words`, in order.
fn bits(words: &[u64]) -> impl Iterator<Item = usize> + '_ {
    words.iter().enumerate().flat_map(|(at, &word)| {
        let mut rest = word;
        std::iter::from_fn(move || {
            if rest == 0 {
                return None;
            }
            let bit = rest.trailing_zeros() as usize;
            rest &= rest - 1;
            Some(at * 64 + bit)
        })
    })
}

/// Sets the bits of `0..domain` in `words` and clears the rest.
fn fill_to(words: &mut [u64], domain: usize) {
    words.fill(!0);
    if domain % 64 != 0
        && let Some(last) = words.last_mut()
    {
        *last = (1u64 << (domain % 64)) - 1;
    }
}

/// A set of locals, a bit each, its words in the run's frame: rustc's `DenseBitSet` without a
/// heap block per set. Every set of a run keeps the domain it was made with, and its bits past
/// the domain stay clear.
struct Set<'f>(&'f mut [u64]);

impl<'f> Set<'f> {
    fn new_empty(frame: &'f Frame, domain: usize) -> Set<'f> {
        Set(frame.alloc_slice_fill_with(domain.div_ceil(64), |_| 0))
    }

    fn new_filled(frame: &'f Frame, domain: usize) -> Set<'f> {
        let words = frame.alloc_slice_fill_with(domain.div_ceil(64), |_| 0);
        fill_to(words, domain);
        Set(words)
    }

    /// A copy of the set in `frame`.
    fn copy_in(&self, frame: &'f Frame) -> Set<'f> {
        Set(frame.alloc_slice_copy(&self.0[..]))
    }

    fn insert(&mut self, at: usize) {
        self.0[at / 64] |= 1u64 << (at % 64);
    }

    fn remove(&mut self, at: usize) {
        self.0[at / 64] &= !(1u64 << (at % 64));
    }

    fn contains(&self, at: usize) -> bool {
        (self.0[at / 64] >> (at % 64)) & 1 != 0
    }

    fn clear(&mut self) {
        self.0.fill(0);
    }

    fn copy_from(&mut self, other: &Set<'_>) {
        self.0.copy_from_slice(&other.0[..]);
    }

    /// Adds `other`'s members; says whether that added any.
    fn union(&mut self, other: &Set<'_>) -> bool {
        let mut added = 0;
        for (mine, &theirs) in self.0.iter_mut().zip(other.0.iter()) {
            added |= theirs & !*mine;
            *mine |= theirs;
        }
        added != 0
    }

    fn intersect(&mut self, other: &Set<'_>) {
        for (mine, &theirs) in self.0.iter_mut().zip(other.0.iter()) {
            *mine &= theirs;
        }
    }

    fn iter(&self) -> impl Iterator<Item = usize> + '_ {
        bits(&self.0[..])
    }

    fn count(&self) -> usize {
        self.0.iter().map(|word| word.count_ones() as usize).sum()
    }
}

impl PartialEq for Set<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.0[..] == other.0[..]
    }
}

fn local_index(local: LocalId) -> usize {
    local.into_raw().into_u32() as usize
}

fn block_index(block: BasicBlockId) -> usize {
    block.into_raw().into_u32() as usize
}

fn is_indirect(place: &StoredPlace) -> bool {
    place.projection.as_slice().contains(&ProjectionElem::Deref)
}

fn index_locals(place: &StoredPlace) -> impl Iterator<Item = usize> + '_ {
    place.projection.as_slice().iter().filter_map(|elem| match elem {
        ProjectionElem::Index(local) => Some(local_index(*local)),
        _ => None,
    })
}

fn operand_place(operand: &Operand) -> Option<&StoredPlace> {
    match &operand.kind {
        OperandKind::Copy(place) | OperandKind::Move(place) => Some(place),
        _ => None,
    }
}

/// The edges out of a block, each with what it does on its own: a call's return writes its
/// destination, a resumed yield its resume argument.
#[derive(Clone, Copy)]
enum Edge<'a> {
    Plain(BasicBlockId),
    Returns(BasicBlockId, &'a StoredPlace),
    Resumes(BasicBlockId, &'a StoredPlace),
}

impl Edge<'_> {
    fn to(self) -> BasicBlockId {
        match self {
            Edge::Plain(to) | Edge::Returns(to, _) | Edge::Resumes(to, _) => to,
        }
    }
}

/// Calls `each` with the edges out of a block that go to a block the body reaches, in order.
fn edges<'a>(body: &MirBody<'_>, terminator: &'a Terminator, mut each: impl FnMut(Edge<'a>)) {
    let mut edge = |it: Edge<'a>| {
        if !body.unreached.contains(&it.to()) {
            each(it);
        }
    };
    match &terminator.kind {
        TerminatorKind::Call { target, destination, .. } => {
            for &to in target.iter() {
                edge(Edge::Returns(to, destination));
            }
        }
        TerminatorKind::Yield { resume, resume_arg, drop, .. } => {
            edge(Edge::Resumes(*resume, resume_arg));
            for &to in drop.iter() {
                edge(Edge::Plain(to));
            }
        }
        kind => {
            for to in kind.successors() {
                edge(Edge::Plain(to));
            }
        }
    }
    if let Some(to) = terminator.kind.unwind() {
        edge(Edge::Plain(to));
    }
}

/// The blocks control reaches from the start.
fn reachable<'f>(frame: &'f Frame, body: &MirBody<'_>) -> &'f [bool] {
    let blocks = body.basic_blocks.len();
    let reached = frame.alloc_slice_fill_with(blocks, |_| false);
    let mut stack = Vec::with_capacity_in(blocks, frame);
    stack.push(body.start_block);
    while let Some(id) = stack.pop() {
        if std::mem::replace(&mut reached[block_index(id)], true) {
            continue;
        }
        if let Some(terminator) = &body.basic_blocks[id].terminator {
            edges(body, terminator, |edge| stack.push(edge.to()));
        }
    }
    reached
}

/// `MaybeBorrowedLocals`: a local some pointer may point into. `Drop` counts, because a
/// destructor is handed `&mut self`.
fn borrow_statement(statement: &StatementKind, state: &mut Set<'_>) {
    match statement {
        StatementKind::Assign(_, Rvalue::Ref(kind, place))
            if *kind != BorrowKind::Shallow && !is_indirect(place) =>
        {
            state.insert(local_index(place.local));
        }
        StatementKind::StorageDead(local) => {
            state.remove(local_index(*local));
        }
        _ => (),
    }
}

fn borrow_terminator(terminator: &TerminatorKind, state: &mut Set<'_>) {
    if let TerminatorKind::Drop { place, .. } | TerminatorKind::DropAndReplace { place, .. } =
        terminator
        && !is_indirect(place)
    {
        state.insert(local_index(place.local));
    }
}

/// A local an operand moves out of as a whole stops requiring storage after it, unless it was
/// borrowed before (`MaybeRequiresStorage`'s primary effect).
fn kill_whole_move(operand: &Operand, borrowed: &Set<'_>, requires: &mut Set<'_>) {
    if let OperandKind::Move(place) = &operand.kind
        && place.projection.as_slice().is_empty()
    {
        let local = local_index(place.local);
        if !borrowed.contains(local) {
            requires.remove(local);
        }
    }
}

/// Calls `each` with the places an inline `asm!` writes: its register outputs, then what it
/// takes.
fn asm_outputs<'a>(statement: &'a StatementKind, mut each: impl FnMut(&'a StoredPlace)) {
    if let StatementKind::InlineAsm(asm) = statement {
        for operand in &asm.operands {
            if let AsmOperand::Reg { output: Some(output), .. } = operand {
                each(output);
            }
        }
        for place in asm.taken.iter() {
            each(place);
        }
    }
}

/// The forward analyses: `MaybeBorrowedLocals` and `MaybeRequiresStorage` at the entry of
/// each block, `None` for a block not reached yet.
struct Forward<'f> {
    borrowed: &'f mut [Option<Set<'f>>],
    requires: &'f mut [Option<Set<'f>>],
}

impl Forward<'_> {
    /// Loads the entry states of block `at` into the buffers, which keep their memory.
    fn load(&self, at: usize, borrowed: &mut Set<'_>, requires: &mut Set<'_>) {
        load_one(&self.borrowed[at], borrowed);
        load_one(&self.requires[at], requires);
    }
}

fn load_one(known: &Option<Set<'_>>, into: &mut Set<'_>) {
    match known {
        Some(known) => into.copy_from(known),
        None => into.clear(),
    }
}

/// Joins `new` into the state an edge flows to; says whether that changed it.
fn join<'f>(frame: &'f Frame, known: &mut Option<Set<'f>>, new: &Set<'_>) -> bool {
    match known {
        Some(known) => known.union(new),
        None => {
            *known = Some(new.copy_in(frame));
            true
        }
    }
}

/// Flows the states at the end of a block along an edge to `to`, queueing `to` if they changed
/// its entry states.
fn flow<'f>(
    frame: &'f Frame,
    forward: &mut Forward<'f>,
    worklist: &mut Vec<BasicBlockId, &'f Frame>,
    to: BasicBlockId,
    borrowed: &Set<'_>,
    requires: &Set<'_>,
) {
    let at = block_index(to);
    let changed =
        join(frame, &mut forward.borrowed[at], borrowed) | join(frame, &mut forward.requires[at], requires);
    if changed {
        worklist.push(to);
    }
}

impl MirBody<'_> {
    /// The locals an `async` block keeps in its future, as rustc's coroutine transform finds
    /// them. The body is the runtime one, its drops elaborated. For a caller outside any query;
    /// one inside a query uses [`MirBody::held_locals_in`] with its frame.
    pub fn locals_held_across_yields(&self) -> HeldLocals {
        blaze_mem::frame::task(crate::db::MIR_TAG, |frame| self.held_locals_in(frame))
    }

    /// [`MirBody::locals_held_across_yields`], its analyses in `frame`.
    pub fn held_locals_in(&self, frame: &Frame) -> HeldLocals {
        let count = self.locals.len();
        let reached = reachable(frame, self);
        let always_live = self.always_storage_live(frame);
        let forward = self.forward(frame, reached);
        let live_out = self.liveness(frame, reached);
        let receiver = self.param_locals.first().copied().map(local_index);

        let mut any = Set::new_empty(frame, count);
        let mut at_yields = Vec::new_in(frame);
        let mut borrowed = Set::new_empty(frame, count);
        let mut requires = Set::new_empty(frame, count);
        for (id, block) in self.basic_blocks.iter() {
            let Some(Terminator { kind: TerminatorKind::Yield { resume, drop, .. }, .. }) =
                &block.terminator
            else {
                continue;
            };
            if !reached[block_index(id)] {
                continue;
            }
            forward.load(block_index(id), &mut borrowed, &mut requires);
            for statement in &block.statements {
                self.storage_statement(statement, &mut borrowed, &mut requires, &mut |_| ());
            }
            let mut live = live_out[block_index(id)].copy_in(frame);
            // An `async` block can't be moved once polled, so a borrow may outlive the use
            // of what it points at: a borrowed local is live until its storage ends.
            live.union(&borrowed);
            live.intersect(&requires);
            if let Some(receiver) = receiver {
                live.remove(receiver);
            }
            // What the body uses in place of a capture is the future's own field, and a
            // promoted constant is no local of rustc's.
            for &(local, _) in &self.upvars_in_place {
                live.remove(local_index(local));
            }
            for &local in &self.promoted_constants {
                live.remove(local_index(local));
            }
            any.union(&live);
            at_yields.push((*resume, *drop, live));
        }

        let mut locals = Vec::with_capacity(any.count());
        locals.extend(
            self.locals.iter().map(|(local, _)| local).filter(|&it| any.contains(local_index(it))),
        );
        let saved: &[usize] = frame.alloc_slice_from_iter(locals.iter().map(|&it| local_index(it)));
        let mut yields = Vec::with_capacity(at_yields.len());
        yields.extend(at_yields.iter().map(|(resume, drop, live)| {
            // `live` is a subset of `any`, so this is its size.
            let mut kept = Vec::with_capacity(live.count());
            kept.extend((0..saved.len()).filter(|&at| live.contains(saved[at])));
            YieldPoint { resume: *resume, drop: *drop, kept: kept.into_boxed_slice() }
        }));
        let conflicts = self.storage_conflicts(frame, reached, &forward, &always_live, &any, saved);
        HeldLocals {
            locals: locals.into_boxed_slice(),
            yields: yields.into_boxed_slice(),
            conflicts,
        }
    }

    /// `always_storage_live_locals`: the locals no `StorageLive` or `StorageDead` mentions.
    fn always_storage_live<'f>(&self, frame: &'f Frame) -> Set<'f> {
        let mut always = Set::new_filled(frame, self.locals.len());
        for (_, block) in self.basic_blocks.iter() {
            for statement in &block.statements {
                if let StatementKind::StorageLive(local) | StatementKind::StorageDead(local) =
                    statement.kind
                {
                    always.remove(local_index(local));
                }
            }
        }
        always
    }

    /// Both forward analyses over one statement. `early` is told of the storage required
    /// after the statement's early effect, which is what storage conflicts are taken from.
    fn storage_statement(
        &self,
        statement: &Statement,
        borrowed: &mut Set<'_>,
        requires: &mut Set<'_>,
        early: &mut impl FnMut(&Set<'_>),
    ) {
        let kind = &statement.kind;
        // Early effect: a borrowed or assigned local needs its storage for the statement.
        borrow_statement(kind, requires);
        match kind {
            StatementKind::StorageDead(local) => {
                requires.remove(local_index(*local));
            }
            StatementKind::Assign(place, _) => {
                requires.insert(local_index(place.local));
            }
            _ => (),
        }
        asm_outputs(kind, |output| requires.insert(local_index(output.local)));
        early(requires);
        // Primary effect: a local moved out of as a whole, and never borrowed before, needs
        // no storage after.
        kind.for_each_operand(&mut |op| kill_whole_move(op, borrowed, requires));
        if let StatementKind::Assign(place, _) = kind {
            requires.insert(local_index(place.local));
        }
        asm_outputs(kind, |output| requires.insert(local_index(output.local)));
        borrow_statement(kind, borrowed);
    }

    /// Both forward analyses over a terminator, up to its edges.
    fn storage_terminator(
        &self,
        terminator: &TerminatorKind,
        borrowed: &mut Set<'_>,
        requires: &mut Set<'_>,
        early: &mut impl FnMut(&Set<'_>),
    ) {
        borrow_terminator(terminator, requires);
        if let TerminatorKind::Call { destination, .. } = terminator {
            requires.insert(local_index(destination.local));
        }
        early(requires);
        if let TerminatorKind::Call { destination, .. } = terminator {
            requires.remove(local_index(destination.local));
        }
        terminator.for_each_operand(&mut |op| kill_whole_move(op, borrowed, requires));
        borrow_terminator(terminator, borrowed);
    }

    fn forward<'f>(&self, frame: &'f Frame, reached: &[bool]) -> Forward<'f> {
        let count = self.locals.len();
        let blocks = self.basic_blocks.len();
        let mut forward = Forward {
            borrowed: frame.alloc_slice_fill_with(blocks, |_| None),
            requires: frame.alloc_slice_fill_with(blocks, |_| None),
        };
        let mut start_requires = Set::new_empty(frame, count);
        for &param in self.param_locals.iter().skip(1) {
            start_requires.insert(local_index(param));
        }
        let start = block_index(self.start_block);
        forward.borrowed[start] = Some(Set::new_empty(frame, count));
        forward.requires[start] = Some(start_requires);
        let mut worklist = Vec::with_capacity_in(blocks, frame);
        worklist.push(self.start_block);
        // Scratch states, reused by every block and edge: no allocation per visit.
        let mut borrowed = Set::new_empty(frame, count);
        let mut requires = Set::new_empty(frame, count);
        let mut edge_requires = Set::new_empty(frame, count);
        while let Some(id) = worklist.pop() {
            if !reached[block_index(id)] {
                continue;
            }
            forward.load(block_index(id), &mut borrowed, &mut requires);
            let block = &self.basic_blocks[id];
            for statement in &block.statements {
                self.storage_statement(statement, &mut borrowed, &mut requires, &mut |_| ());
            }
            let Some(terminator) = &block.terminator else { continue };
            self.storage_terminator(&terminator.kind, &mut borrowed, &mut requires, &mut |_| ());
            edges(self, terminator, |edge| match edge {
                Edge::Plain(to) => {
                    flow(frame, &mut forward, &mut worklist, to, &borrowed, &requires)
                }
                Edge::Returns(to, place) | Edge::Resumes(to, place) => {
                    edge_requires.copy_from(&requires);
                    edge_requires.insert(local_index(place.local));
                    flow(frame, &mut forward, &mut worklist, to, &borrowed, &edge_requires);
                }
            });
        }
        forward
    }

    /// `MaybeLiveLocals`: the locals live at the end of each block, after the effects its
    /// edges have on their own.
    fn liveness<'f>(&self, frame: &'f Frame, reached: &[bool]) -> &'f [Set<'f>] {
        let count = self.locals.len();
        let blocks = self.basic_blocks.len();
        let live_in = frame.alloc_slice_fill_with(blocks, |_| Set::new_empty(frame, count));
        let live_out = frame.alloc_slice_fill_with(blocks, |_| Set::new_empty(frame, count));
        // Scratch states, reused by every block and edge; a changed block's state is swapped
        // into `live_in` and its old one becomes the next scratch.
        let mut state = Set::new_empty(frame, count);
        let mut entry = Set::new_empty(frame, count);
        let mut changed = true;
        while changed {
            changed = false;
            for (id, block) in self.basic_blocks.iter().rev() {
                if !reached[block_index(id)] {
                    continue;
                }
                state.clear();
                if let Some(terminator) = &block.terminator {
                    edges(self, terminator, |edge| match edge {
                        Edge::Plain(to) => {
                            state.union(&live_in[block_index(to)]);
                        }
                        Edge::Returns(to, place) => {
                            entry.copy_from(&live_in[block_index(to)]);
                            if place.projection.as_slice().is_empty() {
                                entry.remove(local_index(place.local));
                            }
                            state.union(&entry);
                        }
                        Edge::Resumes(to, place) => {
                            entry.copy_from(&live_in[block_index(to)]);
                            def_use_write(place, &mut entry);
                            insert_all(&mut entry, index_locals(place));
                            state.union(&entry);
                        }
                    });
                }
                live_out[block_index(id)].copy_from(&state);
                if let Some(terminator) = &block.terminator {
                    live_terminator(&terminator.kind, &mut state);
                }
                for statement in block.statements.iter().rev() {
                    live_statement(&statement.kind, &mut state);
                }
                if live_in[block_index(id)] != state {
                    std::mem::swap(&mut live_in[block_index(id)], &mut state);
                    changed = true;
                }
            }
        }
        live_out
    }

    /// `compute_storage_conflicts`: two held locals conflict if both need their storage at
    /// some statement or terminator of a block that does not end in `Unreachable`. One that
    /// is always storage live conflicts with every other.
    fn storage_conflicts(
        &self,
        frame: &Frame,
        reached: &[bool],
        forward: &Forward<'_>,
        always_live: &Set<'_>,
        held: &Set<'_>,
        saved: &[usize],
    ) -> Box<[u64]> {
        let count = self.locals.len();
        let mut matrix = Conflicts::new(frame, count, always_live, held, saved);
        let mut apply = |state: &Set<'_>| matrix.apply(state);
        let mut borrowed = Set::new_empty(frame, count);
        let mut requires = Set::new_empty(frame, count);
        for (id, block) in self.basic_blocks.iter() {
            if !reached[block_index(id)] {
                continue;
            }
            let Some(terminator) = &block.terminator else { continue };
            if let TerminatorKind::Unreachable = terminator.kind {
                continue;
            }
            forward.load(block_index(id), &mut borrowed, &mut requires);
            for statement in &block.statements {
                self.storage_statement(statement, &mut borrowed, &mut requires, &mut apply);
            }
            self.storage_terminator(&terminator.kind, &mut borrowed, &mut requires, &mut apply);
        }
        matrix.finish()
    }
}

/// The storage conflicts of the held locals, over their positions in `saved` (every local of
/// `held`, in order): `saved.len()²` bits, where rustc's `BitMatrix` has a row of every local
/// per local. Each state costs a word-wise intersection and one row union per held local in it.
struct Conflicts<'a> {
    held: &'a Set<'a>,
    /// A local's position in `saved`, `NOT_HELD` for one not held.
    position: &'a mut [u32],
    /// The held locals that are always storage live, by position: they conflict with all.
    ineligible: Set<'a>,
    together: &'a mut [Set<'a>],
    live_held: Set<'a>,
    eligible: Set<'a>,
}

const NOT_HELD: u32 = u32::MAX;

impl<'a> Conflicts<'a> {
    fn new(
        frame: &'a Frame,
        count: usize,
        always_live: &Set<'_>,
        held: &'a Set<'a>,
        saved: &[usize],
    ) -> Conflicts<'a> {
        let n = saved.len();
        let position = frame.alloc_slice_fill_with(count, |_| NOT_HELD);
        let mut ineligible = Set::new_empty(frame, n);
        for (at, &local) in saved.iter().enumerate() {
            position[local] = at as u32;
            if always_live.contains(local) {
                ineligible.insert(at);
            }
        }
        let together = frame.alloc_slice_fill_with(n, |_| ineligible.copy_in(frame));
        Conflicts {
            held,
            position,
            ineligible,
            together,
            live_held: Set::new_empty(frame, count),
            eligible: Set::new_empty(frame, n),
        }
    }

    /// The locals that need their storage at one point all conflict with each other.
    fn apply(&mut self, state: &Set<'_>) {
        self.live_held.copy_from(state);
        self.live_held.intersect(self.held);
        self.eligible.clear();
        for local in self.live_held.iter() {
            debug_assert_ne!(self.position[local], NOT_HELD);
            self.eligible.insert(self.position[local] as usize);
        }
        for at in self.eligible.iter() {
            self.together[at].union(&self.eligible);
        }
    }

    /// The conflicts as [`HeldLocals`] keeps them: per held local a row of `n.div_ceil(64)`
    /// words, made once at its size.
    fn finish(self) -> Box<[u64]> {
        let n = self.together.len();
        let words = n.div_ceil(64);
        if words == 0 {
            return Box::default();
        }
        // alloc: the result's conflict rows, one block at its size
        let mut conflicts = vec![0u64; n * words].into_boxed_slice();
        for (at, row) in conflicts.chunks_exact_mut(words).enumerate() {
            if self.ineligible.contains(at) {
                fill_to(row, n);
            } else {
                row.copy_from_slice(&self.together[at].0[..]);
            }
        }
        conflicts
    }
}

/// A write to `place` as liveness sees it (`DefUse::for_place` of a store): the whole local is
/// defined, a write through a pointer uses the pointer, a write to a part is neither.
fn def_use_write(place: &StoredPlace, state: &mut Set<'_>) {
    if is_indirect(place) {
        state.insert(local_index(place.local));
    } else if place.projection.as_slice().is_empty() {
        state.remove(local_index(place.local));
    }
}

fn insert_all(state: &mut Set<'_>, locals: impl Iterator<Item = usize>) {
    for local in locals {
        state.insert(local);
    }
}

fn use_place(place: &StoredPlace, state: &mut Set<'_>) {
    state.insert(local_index(place.local));
    insert_all(state, index_locals(place));
}

fn use_operand(operand: &Operand, state: &mut Set<'_>) {
    if let Some(place) = operand_place(operand) {
        use_place(place, state);
    }
}

/// `MaybeLiveLocals` backwards over a statement: what it writes is defined first, then what it
/// reads is used.
fn live_statement(statement: &StatementKind, state: &mut Set<'_>) {
    match statement {
        StatementKind::Assign(place, rvalue) => {
            def_use_write(place, state);
            insert_all(state, index_locals(place));
            match rvalue {
                Rvalue::Ref(_, from)
                | Rvalue::Len(from)
                | Rvalue::Discriminant(from)
                | Rvalue::CopyForDeref(from) => use_place(from, state),
                _ => rvalue.for_each_operand(&mut |it| use_operand(it, state)),
            }
        }
        StatementKind::Deinit(place) => {
            def_use_write(place, state);
            insert_all(state, index_locals(place));
        }
        StatementKind::InlineAsm(_) => {
            asm_outputs(statement, |output| {
                def_use_write(output, state);
                insert_all(state, index_locals(output));
            });
            statement.for_each_operand(&mut |it| use_operand(it, state));
        }
        StatementKind::FakeRead(_)
        | StatementKind::StorageLive(_)
        | StatementKind::StorageDead(_)
        | StatementKind::Nop => (),
    }
}

/// `MaybeLiveLocals` backwards over a terminator. A call's destination and a yield's resume
/// argument are written on the edges, not here.
fn live_terminator(terminator: &TerminatorKind, state: &mut Set<'_>) {
    match terminator {
        TerminatorKind::Call { destination, .. } => {
            if is_indirect(destination) {
                state.insert(local_index(destination.local));
            }
            insert_all(state, index_locals(destination));
        }
        TerminatorKind::Drop { place, .. } => use_place(place, state),
        TerminatorKind::DropAndReplace { place, .. } => {
            def_use_write(place, state);
            use_place(place, state);
        }
        _ => (),
    }
    terminator.for_each_operand(&mut |it| use_operand(it, state));
}

#[cfg(test)]
mod tests {
    use blaze_mem::{Frame, Tag, frame};
    use rustc_index::bit_set::DenseBitSet;

    use super::{Conflicts, Set, bits, row};

    fn dense(count: usize, members: &[usize]) -> DenseBitSet<usize> {
        let mut set = DenseBitSet::new_empty(count);
        for &it in members {
            set.insert(it);
        }
        set
    }

    fn of<'f>(frame: &'f Frame, count: usize, members: &[usize]) -> Set<'f> {
        let mut set = Set::new_empty(frame, count);
        for &it in members {
            set.insert(it);
        }
        set
    }

    /// The matrix as it was computed before [`Conflicts`]: a row of every local per local, in
    /// rustc's `DenseBitSet`s.
    fn model(count: usize, always_live: &[usize], held: &[usize], states: &[Vec<usize>]) -> Vec<bool> {
        let held_set = dense(count, held);
        let mut ineligible = dense(count, always_live);
        ineligible.intersect(&held_set);
        let mut together: Vec<DenseBitSet<usize>> = vec![ineligible.clone(); count];
        for state in states {
            let mut eligible = dense(count, state);
            eligible.intersect(&held_set);
            for local in eligible.iter() {
                together[local].union(&eligible);
            }
        }
        let n = held.len();
        let mut conflicts = vec![false; n * n];
        for (a, &local_a) in held.iter().enumerate() {
            for (b, &local_b) in held.iter().enumerate() {
                conflicts[a * n + b] =
                    ineligible.contains(local_a) || together[local_a].contains(local_b);
            }
        }
        conflicts
    }

    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        /// The members of a set of `count` locals, each in it one time in `one_in`, in order.
        fn members(&mut self, count: usize, one_in: u64) -> Vec<usize> {
            (0..count).filter(|_| self.next() % one_in == 0).collect()
        }
    }

    #[test]
    fn conflicts_over_held_positions_are_the_full_matrix() {
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        for round in 0..200 {
            let count = [0, 1, 2, 63, 64, 65, 130][round % 7] + (rng.next() % 40) as usize;
            let density = 1 + rng.next() % 4;
            let held = rng.members(count, density);
            let density = 1 + rng.next() % 8;
            let always_live = rng.members(count, density);
            let mut states = Vec::new();
            for _ in 0..rng.next() % 12 {
                let density = 1 + rng.next() % 5;
                states.push(rng.members(count, density));
            }
            let expected = model(count, &always_live, &held, &states);
            let got = frame::task(Tag::Mir, |frame| {
                let held_set = of(frame, count, &held);
                let always = of(frame, count, &always_live);
                let mut matrix = Conflicts::new(frame, count, &always, &held_set, &held);
                for state in &states {
                    matrix.apply(&of(frame, count, state));
                }
                matrix.finish()
            });
            let n = held.len();
            for a in 0..n {
                let row = row(&got, n, a);
                let listed: Vec<usize> = bits(row).collect();
                let wanted: Vec<usize> = (0..n).filter(|&b| expected[a * n + b]).collect();
                assert_eq!(listed, wanted, "round {round}: {count} locals, {n} held, row {a}");
            }
        }
    }

    #[test]
    fn frame_sets_are_dense_bit_sets() {
        let mut rng = Rng(0x2545_f491_4f6c_dd1d);
        for round in 0..300 {
            let count = [0, 1, 63, 64, 65, 129][round % 6] + (rng.next() % 70) as usize;
            let density = 1 + rng.next() % 4;
            let a = rng.members(count, density);
            let density = 1 + rng.next() % 4;
            let b = rng.members(count, density);
            frame::task(Tag::Mir, |frame| {
                let (mut x, y) = (of(frame, count, &a), of(frame, count, &b));
                let (mut dx, dy) = (dense(count, &a), dense(count, &b));
                assert_eq!(x.union(&y), dx.union(&dy), "round {round}");
                assert_eq!(x.iter().collect::<Vec<_>>(), dx.iter().collect::<Vec<_>>(), "round {round}");
                assert_eq!(x.count(), dx.count(), "round {round}");
                x.intersect(&of(frame, count, &a));
                dx.intersect(&dense(count, &a));
                assert_eq!(x.iter().collect::<Vec<_>>(), dx.iter().collect::<Vec<_>>(), "round {round}");
                assert_eq!(x == of(frame, count, &a), dx == dense(count, &a), "round {round}");
                let filled = Set::new_filled(frame, count);
                assert_eq!(
                    filled.iter().collect::<Vec<_>>(),
                    DenseBitSet::<usize>::new_filled(count).iter().collect::<Vec<_>>(),
                    "round {round}",
                );
            });
        }
    }
}
```

What the test holds: `conflicts_over_held_positions_are_the_full_matrix` checks the bit rows (what
`conflict` and `conflicts_of` read) against the old full-matrix computation over rustc's
`DenseBitSet`; `frame_sets_are_dense_bit_sets` checks each `Set` operation the analyses use
against `DenseBitSet` at word boundaries (0, 1, 63, 64, 65, 129 and more). The analyses' results
end to end are held by the existing coroutine layout tests (the layouts are compared with rustc's),
which run through `layout_of_ty` → `held_locals_in` unchanged in meaning.

A further, optional saving: `HeldLocals` is recomputed for the same body by the layout and by the
three codegen/items callers (3 to 4 times per `async` body). A memoized query over the runtime
coroutine body would make it once; that is a new query in `db.rs` (mem-db-infer's) and the
callers' files, so it is only noted here.

> forwarded to db (from requests, pass 17; told by tmux).
> applied by db-mem-infer-b-eval (21:17): `mir/held.rs` as written, except the tests draw from `blaze_ids::rng::SplitMix64` (the file's tests had moved to it; a `members` helper in place of the local `Rng`), and `// alloc:` notes on the result's three exact-size lists. `task(crate::db::MIR_TAG, ..)` as written. db.md's "dedup: bits: hir-ty `mir/held.rs`" had not landed; this replaces its `DenseBitSet` set with frame sets, so that section is met (no `DenseBitSet` outside the tests' reference). Waits on mem-db-infer.md 2 (`allocator_api`) and 3 (`MIR_TAG`), db-mem-infer-a's.

## `target_feature.rs` and `target_feature/tests.rs`

> forwarded to db (from requests, pass 17; told by tmux).
> applied by db-mem-infer-b-eval (21:43): F1 and F2, below.

### F1. `Declared::of` collects in the frame, makes the memo once (R1, R4, R6)

Lines 338-357. Before:

```rust
impl Declared {
    fn of<'a>(names: impl IntoIterator<Item = &'a Symbol>) -> Self {
        let mut known = TargetFeatureSet::EMPTY;
        let mut unknown = Vec::new();
        for name in names {
            match TargetFeature::from_name(name.as_str()) {
                Some(feature) => known.insert(feature),
                None => unknown.push(name.clone()),
            }
        }
        unknown.sort_unstable_by(|a, b| a.as_str().cmp(b.as_str()));
        unknown.dedup();
        Self { known, unknown: unknown.into_boxed_slice() }
    }
}

#[salsa::tracked(returns(ref))]
fn declared_target_features(db: &Db, owner: FunctionId) -> Declared {
    Declared::of(AttrFlags::target_features(db, owner))
}
```

After:

```rust
impl Declared {
    fn of<'a>(frame: &Frame, names: impl IntoIterator<Item = &'a Symbol>) -> Self {
        let mut known = TargetFeatureSet::EMPTY;
        let mut unknown = Vec::new_in(frame);
        for name in names {
            match TargetFeature::from_name(name.as_str()) {
                Some(feature) => known.insert(feature),
                None => unknown.push(name),
            }
        }
        unknown.sort_unstable_by(|a, b| a.as_str().cmp(b.as_str()));
        unknown.dedup();
        // alloc: the memo's own list, at its size; none when every name is known
        // clone: the memo keeps its own symbols
        let unknown = unknown.iter().map(|&name| name.clone()).collect();
        Self { known, unknown }
    }
}

#[salsa::tracked(returns(ref), mem_tag = crate::db::INFER_TAG)]
fn declared_target_features(db: &Db, owner: FunctionId) -> Declared {
    blaze_mem::frame::current(|frame| Declared::of(frame, AttrFlags::target_features(db, owner)))
}
```

with `use blaze_mem::Frame;`. The sort and `dedup` run on `&Symbol`s (equal symbols are equal
references' targets, so `dedup` keeps the same names), and only the kept ones are cloned, into a
`Box<[Symbol]>` from a `slice::Iter` map: one exact block, none when empty, where before the `Vec`
grew and `into_boxed_slice` shrank it. Same set of names in the same order.

Test, `target_feature/tests.rs:235`. Before:

```rust
    let declared: Vec<Declared> = olds.iter().map(|(raw, _)| Declared::of(raw)).collect();
```

After:

```rust
    let declared: Vec<Declared> = blaze_mem::frame::task(blaze_mem::Tag::Infer, |frame| {
        olds.iter().map(|(raw, _)| Declared::of(frame, raw)).collect()
    });
```

It compares every declared set against rustc's expansion of the same names, so it holds the
change.

> forwarded to db (from requests, pass 17; told by tmux).
> applied by db-mem-infer-b-eval (21:19): as written, with `mem_tag = Infer` (salsa's option as filed in mem-db-salsa M2 takes the bare variant; the amended form takes either) in place of `crate::db::INFER_TAG`. Also `target_feature/tests.rs:178-179` (`Declared::of` in `is_target_feature_call_safe`'s check, not named here) take a frame from one `task(Tag::Infer, ..)`. dedup-bits' `TargetFeatureSet` change had landed and is kept.

### F2. Tags of the other two (R6)

`crate_target_arch` (line 148) and `crate_target_features` (line 361):
`#[salsa::tracked(returns(copy), mem_tag = crate::db::INFER_TAG)]`. Both memos are `Copy`
values; `crate_target_features`' `collect` is into a `TargetFeatureSet` (a bit set by value) and
allocates nothing. `TargetFeatures::from_fn` and the `enables*` / `is_superset` checks are the hit
path (R7): they read the two memos and allocate nothing.

> forwarded to db (from requests, pass 17; told by tmux).
> applied by db-mem-infer-b-eval (21:19): both take `mem_tag = Infer` (the bare variant, as salsa's option is filed), not `crate::db::INFER_TAG`. `crate_target_features`' `collect` into the bit set gets `// alloc: none, the set is bits by value` for the hot lint.

## `solver_errors.rs`: a cold path

Error reporting runs only for a body with a trait error; with none, it allocates nothing. Every
allocation in it builds a diagnostic the caller keeps (most already carry `// alloc: error path`).
One more mark, line 405 (`handle_trait_unimplemented`), before:

```rust
    let mut parent_trait_predicates = error
        .parent_trait_obligations
```

after:

```rust
    // alloc: error path, the diagnostic's own list
    let mut parent_trait_predicates = error
        .parent_trait_obligations
```

`elaborate`'s sets are the solver's (mem-db-solver's).

> forwarded to db (from requests, pass 17; told by tmux).
> applied by db-mem-infer-b-eval (21:20): the mark, as a trailing comment on the statement's `.collect::<Vec<_>>();` line (418) rather than above its first line (406): `hot_lint.py` reads a mark only on the flagged line or the one above it, and the file's other error-path marks trail their line.

## `evict.rs`, `intrinsic.rs`, `lang_items.rs`, `primitive.rs`, `mir/target.rs`

No change. `evict.rs`' `drop_inference`, `drop_all_inference`, `drop_statics`,
`drop_all_const_eval` and `drop_all` take `&mut Db` outside any query and make no temporaries
(salsa's `evict_values*` are mem-db-salsa's). `intrinsic.rs`' `intrinsic_of`,
`atomic_orderings` and `atomic_ordering_of` read a static `KnownNames` table and `match`; the
`name.clone()` in `intrinsic_of` is its error. `lang_items.rs` and `primitive.rs` are `match`es.
`mir/target.rs` is an atomic word read and written by the driver.

> forwarded to db (from requests, pass 17; told by tmux).
> applied by db-mem-infer-b-eval (21:20): nothing to apply (no change asked).

## Tracked fns of these files and their tags (R6)

| file:line | fn | tag | memo | built from |
|---|---|---|---|---|
| `layout.rs:174` | `struct_flags` | `INFER_TAG` | `StructFlags` (`Copy`), 0 blocks | the signature memo |
| `layout.rs:183` | `struct_tail_may_be_unsized` | `INFER_TAG` | `bool` | an `InferCtxt` (solver's) |
| `layout.rs:195` | `variant_field_ids` | `INFER_TAG` | `Box<[LocalFieldId]>`, 1 block (0 if no fields) | exact `Vec` (L7) |
| `layout.rs:201` | `enum_variant_ids` | `INFER_TAG` | `Box<[EnumVariantId]>`, 1 block | exact `Vec` (L7) |
| `layout.rs:377` | `layout_of_ty_query` | `INFER_TAG` | `Result<Interned<InternedLayout>, LayoutError>`: a handle; 1 arena node per distinct layout | frame field lists (L2 to L5), `held` in the frame |
| `layout/adt.rs:27` | `layout_of_adt_query` | `INFER_TAG` | as above | frame `fields`/`ends` (A1), frame variant slices (A2) |
| `layout/target.rs:8` | `target_data_layout_query` | `INFER_TAG` | `Result<TargetDataLayout, ..>`, parsed once per crate | the target spec |
| `consteval.rs:431` | `const_eval_discriminant_variant` | `INFER_TAG` | `Result<i128, ConstEvalError>` | the evaluator's interned allocation |
| `consteval.rs:503` | `const_eval_query` | `INFER_TAG` | `Result<StoredAllocation, ..>`, a handle | the evaluator |
| `consteval.rs:548` | `anon_const_eval_query` | `INFER_TAG` | as above | the evaluator |
| `consteval.rs:588` | `const_eval_static_query` | `INFER_TAG` | as above | the evaluator |
| `target_feature.rs:148` | `crate_target_arch` | `INFER_TAG` | `TargetArch` (`Copy`) | the crate's cfg |
| `target_feature.rs:354` | `declared_target_features` | `INFER_TAG` | `Declared`: a bit set + `Box<[Symbol]>`, 0 blocks unless a name is unknown | frame `Vec<&Symbol>` (F1) |
| `target_feature.rs:361` | `crate_target_features` | `INFER_TAG` | `TargetFeatureSet` (`Copy`) | the crate's cfg |

`mir.rs` and `mir/held.rs` have no tracked fns (the MIR queries are `HirDatabase` methods in
`db.rs`, implemented in hir-ty-mir); `MIR_TAG` is used by `locals_held_across_yields`' task.

> forwarded to db (from requests, pass 17; told by tmux).
> applied by db-mem-infer-b-eval (21:43): lane db's seven (`consteval.rs` 4, `target_feature.rs` 3) take `mem_tag = Infer` (C1, F1, F2); the layout ones are lane items' (applied by requests-items-mem, also `mem_tag = Infer`).

## Entry points called outside any query (R1)

| fn | callers | frame |
|---|---|---|
| `MirBody::locals_held_across_yields` (`mir/held.rs`) | `blaze-items/src/form.rs:1692`, `hir-ty-codegen/debuginfo/types.rs:618`, `hir-ty-codegen/mod.rs:10332` | `task(MIR_TAG, ..)` (H1); in-query callers use `held_locals_in` |
| `MirBody::local_to_binding_map` (`mir.rs`) | hir-ide `mir_pretty`, codegen `debuginfo/types.rs:621`, `v2/hashes.rs:265` | none needed: one exact block, the caller's (M4) |
| `MirBody`'s `Hash` (`hash_by_key`) | codegen hashing | none: scratch buffer (M2) |
| `TerminatorKind::successors` | every CFG walk (M1) | none: allocates nothing |
| `struct_tail_erasing_lifetimes` (`layout.rs`) | borrowck `check.rs:1051`, codegen `debuginfo/types.rs:509` | none: allocates nothing |
| `intrinsic_of`, `atomic_orderings`, `atomic_ordering_of` (`intrinsic.rs`) | codegen, MIR building | none: allocate nothing |
| `evict.rs`' five `drop_*` | the driver, with `&mut Db` | none: no temporaries |
| `trace_consts_with` (`consteval.rs`) | the driver, once | none |

> forwarded to db (from requests, pass 17; told by tmux).
> applied by db-mem-infer-b-eval (21:43): the one that opens a task is H1's `locals_held_across_yields` (in); the rest need none, as listed.

## Hit-path helpers (R7)

None of them allocates, so none needs the caller's frame: `try_const_usize`, `evaluated`, the
`const_eval*` wrappers on `Ok`, `HeldLocals::contains` / `conflict` / `conflicts_of`,
`TargetFeatures::from_fn` and its `enables*`, `field_layout` (a memo read), `is_by_move_coroutine`.

> forwarded to db (from requests, pass 17; told by tmux).
> applied by db-mem-infer-b-eval (21:43): nothing to apply (none allocates).

## Every remaining site, accounted for

Of `.clone()`, `Vec::new()`, `vec![`, `.collect()`, `.to_vec()`, `Box::new(`, `FxHashMap::default()`,
`FxHashSet::default()`, `SmallVec`, `ThinVec::new()`, `IndexSet`/`IndexMap` in these files after
the entries above:

- `layout.rs`: `trait_env.clone()` (`StoredParamEnvAndCrate` is a `Copy` row: free);
  `substitution: subst.clone()` is inside a comment; the Pat branch's
  `db.layout_of_ty(..)?.0.clone()` copies the base layout to adjust and intern it (the result);
  `offsets`/`in_memory_order` `[..].into_iter().collect()` are the result's one-element lists;
  `body.locals[local].ty.clone()` is a `StoredTy` handle; tests' `collect`/`to_vec`/`clone`.
- `layout/adt.rs`: `trait_env.clone()` (free); tests.
- `layout/target.rs`: the error `format!` and `e.clone()`.
- `consteval.rs`: the result-building ones listed in C3; `Err(err.clone())` in the wrappers.
- `mir.rs`: `SwitchTargets`' `SmallVec`s (M6), `smallvec![value]` in `static_if` (inline, never
  spills); `place.clone()` in `mark_moves`; error `String`s; the owned body's lists (built by
  hir-ty-mir, M5); tests.
- `mir/held.rs`: `vec![0u64; n * words]` in `finish` (the result, marked); the exact
  `Vec::with_capacity`s that become the result's `Box<[T]>`s; tests.
- `target_feature.rs`: F1's `collect` and `clone` (the memo, marked).
- `solver_errors.rs`: the diagnostics' own lists (cold path).

> forwarded to db (from requests, pass 17; told by tmux).
> applied by db-mem-infer-b-eval (21:43): each site of lane db's files that stays now carries its reason as a `// alloc:` / `// clone:` mark (C3, M1, M6, F2, `solver_errors.rs`, `intrinsic.rs`), and `hot_lint.py` reports nothing in `consteval.rs`, `mir.rs`, `mir/`, `intrinsic.rs`, `target_feature.rs`, `solver_errors.rs`, `evict.rs`, `lang_items.rs`, `primitive.rs`.

## Speed

The temporaries these files make are per layout, per `async` body and per CFG walk rather than
per expression; for a 1,000-expression body (about 300 blocks and 1,000 locals once lowered, one
`async` block in it, 20 multi-way `match`es, a dozen distinct tuple and enum types laid out):

| temporary | per run, before | per run, after |
|---|---|---|
| `held` sets: `live_in` and `live_out` (`2B`), forward states (up to `2B`), 7 scratch | about 1,207 global blocks of 128 bytes (`⌈1000/64⌉ = 16` words) + `Vec` headers: about 24 µs at 20 ns each | about 155 KB of frame (`(4B + 7) × 128` bytes + 16-byte slice headers), about 1,207 bumps: about 2.4 µs at 2 ns each |
| `held` conflicts, `n = 50` held locals | 50 `DenseBitSet`s + `Vec<bool>` of 2,500 + the result `Vec`s' growth: about 60 blocks | frame rows (`50 × 8` bytes) + 400-byte result: 1 block, plus 1 per result list |
| `held` per `async` body, all callers | 3 to 4 runs of the above | the same runs, about 10× cheaper each |
| `successors` over all CFG walks | about 10 walks × 20 multi-way switches = 200 global blocks, about 4 µs | 0 |
| `univariant` of a 12-field tuple | 2 global blocks (both `SmallVec`s spill) | 96 + 96 frame bytes |
| `layout_of_adt` of an enum with `V = 10` variants and `F = 30` fields | 3 `SmallVec` spills + `1 + V` `IndexVec`s: about 14 global blocks, about 280 ns | `8F + 4V = 280` frame bytes (A1); after R-abi `16F + 20V = 680` frame bytes, 0 global |
| coroutine layout, 50 held locals, 5 yields | about 6 + `V` blocks | `prefix` and the `BitMatrix` only (2 blocks; 1 more per variant until R-abi) |
| `hash_by_key`, more than 8 upvar bindings | 1 global block per hash | 0 (scratch buffer, kept warm) |
| `Declared::of` with `k` unknown names | `⌈log₂ k⌉ + 1` blocks (growth + shrink) | 1 exact (0 when `k = 0`, the usual case) |

Frame allocation is about 2 ns against 15 to 25 ns for the general allocator, and frame memory is
freed all at once when the task ends. The dominant temporaries are `held`'s bit sets (about 155 KB
of frame per `async` body of this size) and the CFG walks' successor lists. These figures assume
the frame-depth objection above is resolved: until it is, a query task nested deeper than 8 pays
a map and unmap of its frame (microseconds), which would outweigh every saving in this file for
deep layout and const-eval chains.

> forwarded to db (from requests, pass 17; told by tmux).
> applied by db-mem-infer-b-eval (21:43): nothing to apply (the bound; it holds for the entries as applied).
