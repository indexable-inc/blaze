---
status: plan
summary: speed-types-probe-layout's notes on `crates/hir-ty/src/layout.rs` and `layout/adt.rs`, the `layout_of_ty` and `layout_of_adt` queries codegen asks for every type it moves. They cover the benchmark (rustc's `layout_of`), what a layout cost before and after the change, and what is left, including what interning layouts would take.
date: 2026-10-29
---
# speed-types-probe-layout: type layouts

Notes of `speed-types-probe-layout` (a child of [speed-types-probe](speed-types-probe.md), lane
[items](../items.md)) on `crates/hir-ty/src/layout.rs`, `layout/adt.rs` and `layout/target.rs`.

## The benchmark

rustc's `layout_of` (`rustc_ty_utils/src/layout.rs`) is the fastest known design for this:

- It runs `normalize_erasing_regions` only when the type has aliases (`has_aliases`). Most types
  codegen lays out have none, and they never reach the trait solver.
- It collects each variant's field layouts once and hands them to the `LayoutCalculator` as
  borrowed slices.
- It interns the result (`tcx.mk_layout`), so a layout is a pointer, compared by address. It is
  computed once per `(type, typing env)` and never cloned.
- Struct facts that don't depend on the generic arguments come from per-definition queries
  (`adt_sized_constraint`), not from each instantiation.

## Before

For each call of `layout_of_ty` (one per distinct type and environment):

- `DbInterner::new_with` ran on every call: two salsa reads (`lang_items`, `crate_def_map`),
  and so two dependency edges on every layout. An inference context was built on every call
  too, and `deeply_normalize` ran over the type whatever it was: a `FulfillmentCtxt`, a fold
  and an obligation drain, even for `u8`.
- A tuple or closure made three heap `Vec`s: the `Arc<Layout>`s, the `&Layout`s, and an
  `IndexVec` of `&&Layout`. A coroutine made four.

For each call of `layout_of_adt`:

- Three heap allocations per variant: a `Vec<Arc<Layout>>`, a `Vec<&Layout>` and an
  `IndexVec`.
- One `field_types` lookup and one `DbInterner` per field, not per variant.
- On every instantiation of a struct with fields, a `Sized` check of its last field's identity
  type: `DbInterner::new_with` (two salsa reads), an inference context, `trait_environment`, and
  a `Sized` goal through the solver. The answer depends only on the struct and the crate.
- `enum_variant_ids` looked up once per variant while building the discriminants, and
  `struct_flags` twice.

## The change

- `layout_of_ty` normalizes through a `Normalizer`. It returns a type with no aliases as it is.
  It builds the interner and inference context once, on the first type that has an alias or
  unevaluated constant: the top type, or a pointee's tail field. Types after that share the same
  context, as before. The answers are exactly the same: `NormalizationFolder::try_fold_ty` and
  `try_fold_const` return at once on `!has_aliases()`. `HAS_ALIAS` (`vendor/rustc_type_ir/src/flags.rs`)
  includes `HAS_CT_PROJECTION`, which `ConstKind::Unevaluated` sets. On an empty fulfillment
  context, `evaluate_obligations_error_on_ambiguity` reports nothing. The `!has_escaping_bound_vars`
  assertion `deeply_normalize` made is kept. An alias-free layout now runs no trait-solver code
  and has no dependency on `lang_items` or `crate_def_map`.
- Tuples and closures use `univariant`: the field layouts go in a `SmallVec<[Arc<Layout>; 8]>`,
  the references in two `SmallVec`s, and the calculator gets a borrowed
  `IndexSlice::from_raw`. That means no heap allocation up to eight fields. A coroutine's
  upvars, prefix and locals use the same stack buffers, with the locals borrowed as an
  `IndexSlice`.
- `layout_of_adt` puts every variant's field layouts in one flat `SmallVec` and records where
  each variant ends. It reads `field_types` and builds a `DbInterner` once per variant, reads
  `enum_variant_ids` and `struct_flags` once, and computes the discriminant size once. The
  calculator's signature (`&IndexSlice<VariantIdx, IndexVec<FieldIdx, F>>`) needs an owned
  `IndexVec` per variant, so that is the one allocation left per variant (exact-sized, and none
  for a variant without fields).
- The struct-tail `Sized` check is now the tracked query `struct_tail_may_be_unsized(StructId,
  Crate)`, kind `0x2457` (requested in `v2/requests/db-ty.md`). It runs once per struct and
  crate rather than once per instantiation. It also takes `trait_environment` and the last
  field's type out of `layout_of_adt`'s dependencies.
- `target.rs`: `target_data_layout` is one tracked query per crate. It is parsed once and
  returned by reference, and each layout reads it once. No change was needed.

## After

| per call | before | after |
| --- | --- | --- |
| `layout_of_ty`, alias-free type | interner (2 salsa reads), infer ctxt, a normalizing fold | none of these |
| `layout_of_ty`, tuple or closure of 8 fields or fewer | 3 heap allocations | 0 |
| `layout_of_adt`, variant with fields | 3 heap allocations, 1 `field_types` read per field | 1 allocation, 1 read per variant |
| `layout_of_adt`, struct instantiation | a `Sized` goal through the solver | one memoized `bool` read |

This matches rustc on the normalization skip, the per-variant collection and the per-definition
`Sized` fact.

## Still short of the benchmark, and what it would take

- **Interning layouts.** `layout_of_ty` returns `Result<Arc<Layout>, LayoutError>` with
  `returns(clone)`, so each field read costs an atomic `Arc` increment and the `Arc` is kept
  per field. Interning would mean a `LayoutId(NonZeroU32)` into a global append-only table,
  shaped like `next_solver`'s `TYS`, deduplicated by an `FxHash` of the `LayoutData`. Each query
  would return the id, and readers would go through `id.get() -> &'static Layout`. That changes
  the value type that `hir-ty-mir` (`mir/eval.rs`, `mir/lower.rs`, `mir/transform.rs`),
  `hir-ty-codegen` (`mod.rs`, `sve.rs`, `debuginfo/types.rs`), `hir-ty-borrowck`, `hir`,
  `blaze-items` and `blaze-diagnostics` read, so it is left as is, per the brief. With interning,
  the `SmallVec<[Arc<Layout>; 8]>` buffers become `SmallVec<[&'static Layout; 8]>`, and one
  buffer goes away.
- **One `IndexVec` per variant** is left because of `rustc_abi`'s `layout_of_struct_or_enum`
  and `layout_of_union` signatures (`crates/rustc_abi`, not my path). Taking
  `&IndexSlice<VariantIdx, &IndexSlice<FieldIdx, F>>` would let a whole ADT go through stack
  buffers.
- **`AttrFlags::repr`** is read in `layout_of_ty` (simd and scalable checks) and again in
  `layout_of_adt`. Both are memoized salsa reads, and the check can't cross the query
  boundary without changing `layout_of_adt`'s signature.

## Tests

- `layout::tests::univariant_on_the_stack_matches_the_vecs`: 400 seeded cases of 0 to 12
  fields, plus an unsized tail. The fields cover niches (`bool`, `char`, a reference), integers,
  a ZST, `!`, an array and a wide pointer. Kinds are `AlwaysSized` (closures) and
  `MaybeUnsized` (tuples). The stack path must equal the old three-`Vec` path.
- `layout::tests::skipping_normalization_is_exactly_the_alias_free_types`: over tuples,
  closures, references, raw pointers, arrays, projections, nested projections and unevaluated
  array lengths, `needs_normalizing` must equal a structural visitor that looks for any
  `TyKind::Alias` or `ConstKind::Unevaluated`.
- `layout::adt::tests::one_flat_buffer_matches_a_buffer_per_variant`: 400 seeded structs (some
  with an unsized tail), unions, and enums of 0 to 6 variants with niche-bearing fields. The
  flat buffer must give the same `layout_of_struct_or_enum` and `layout_of_union` as the old
  nested buffers.
- The database-level layout tests (`crates/hir-ide/src/tests/layout.rs`) and
  `blaze-items`' `renaming_a_field_leaves_the_layout` / `changing_a_field_type_reruns_the_layout`
  still hold. The new query returns the same `bool` after a field rename, so it backdates.
