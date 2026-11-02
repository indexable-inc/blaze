# Requests of db-ty-lower

## 1. db-ty-db: `crates/hir-ty/src/db.rs`, the signature accessors return `Lowered` views

What: in `trait HirDatabase` (around lines 342-396), change four return types and their bodies'
nothing else (the bodies already call `crate::lower::*`, which return the new types):

```rust
fn impl_self_ty_with_diagnostics<'db>(&'db self, def: ImplId)
    -> crate::lower::Lowered<'db, StoredEarlyBinder<StoredTy>>;
fn impl_trait_with_diagnostics<'db>(&'db self, def: ImplId)
    -> Option<crate::lower::Lowered<'db, StoredEarlyBinder<StoredTraitRef>>>;
fn field_types_with_diagnostics<'db>(&'db self, var: VariantId)
    -> crate::lower::Lowered<'db, FieldTypes>;
fn field_types(&self, var: VariantId) -> &FieldTypes;
```

and in `signature_anon_consts_and_diagnostics` (around line 709) make `push` take the view, so
both a per-item result and a batch row fit:

```rust
fn push<'db, T: ?Sized>(
    result: &mut ArrayVec<(&'db [AnonConstId<'db>], &'db [TyLoweringDiagnostic]), 5>,
    item: crate::lower::Lowered<'db, T>,
) {
    result.push((item.defined_anon_consts(), item.diagnostics()));
}
```

with each call that passes a `&TyLoweringResult` passing `.as_lowered()`
(`db.generic_defaults_with_diagnostics(def).as_lowered()`, `const_param_types_with_diagnostics`,
`type_for_type_alias_with_diagnostics`, `type_alias_bounds_with_diagnostics`,
`fn_sig_for_fn_with_diagnostics`, `type_for_const_with_diagnostics`,
`type_for_static_with_diagnostics`), and the three that return `Lowered` already
(`GenericPredicates::query_with_diagnostics(db, def)`, `db.impl_self_ty_with_diagnostics(id)`,
`db.impl_trait_with_diagnostics(id)`, the last one's `if let Some(trait_ref)`) passed as they are.
The `ArenaMap`, `LocalFieldId` and `FieldType` imports may become unused.

Why: `field_types_with_diagnostics`, `impl_self_ty_with_diagnostics`,
`impl_trait_with_diagnostics` and `GenericPredicates::query_with_diagnostics` read a dependency
crate's module-level items from a per-crate batch (`lower/batch.rs`), whose diagnostics are in a
side table: there is no `TyLoweringResult` to borrow. `Lowered` is a `Copy` view of value,
diagnostics and constants that both give. `FieldTypes` is a `repr(transparent)` slice
(`[FieldType]`, one per field in `LocalFieldId` order): a range of the batch's CSR column, or a
per-item query's `ThinVec`. It has the `ArenaMap` methods callers use (`iter()` yielding
`(LocalFieldId, &FieldType)`, `values()`, `get(id)`, `[id]`, `len()`, `&FieldTypes:
IntoIterator`), so the ~60 `field_types(..)` callers in hir-ty, hir, hir-ty-mir and hir-ide
compile unchanged; `TyLoweringResult<'db, FieldTypes>` (today's declaration) does not compile at
all, `FieldTypes` being unsized.

Callers outside db.rs that use only `.diagnostics()` and `.defined_anon_consts()` (which
`Lowered` has) and need no change: `crates/hir/src/diagnostics.rs:1556`,
`crates/hir/src/semantics.rs:2209`, `crates/blaze-items/src/facts.rs:578`,
`crates/hir-ide/src/tests.rs:162`.

> forwarded to db: `crates/hir-ty/src/db.rs` is inside lane db (db-ty-db); `field_types` still returns `&ArenaMap`.
> checked by db-audit-dbsub-tysmall (22:33): in the tree at `crates/hir-ty/src/db.rs:340-366` (the four signatures return `Lowered`/`&FieldTypes`) and `:1200` (`push` takes `Lowered`, callers at :1171-1193 pass `.as_lowered()` or the `Lowered` as is).

## 2. db-ty-db: `crates/hir-ty/src/lib.rs`, re-export the new public types

What: add to the `pub use lower::{..}` at line 97:

```rust
FieldTypes, Lowered,
batch::{Batch, CrateFieldTypes, CrateImplPredicates, CrateImplSelfTys, CrateImplTraits, SideTable},
persist::{BatchKind, ColdBatch, ColdBatchRef, PredicatesRec},
```

Why: `mod lower` is private, so `FieldTypes` (request 3's caller), `Lowered` (request 1's
signatures), the batches (`CrateFieldTypes::of(db, krate)` and the like, for whoever keeps
them cold) and their cold form (`ColdBatch::of_field_types(interner, batch)`,
`ColdBatch::read(buffer)?.field_types(interner)`, `blaze_format::Persist`) are not reachable
from other crates.

> forwarded to db: `crates/hir-ty/src/lib.rs` is inside lane db; not re-exported yet.
> checked by db-audit-dbsub-tysmall (22:33): in the tree at `crates/hir-ty/src/lib.rs:113-125` (`FieldTypes`, `Lowered`, the five `batch::` types and the four `persist::` types).

## 3. Coordinator (hir-ide): `crates/hir-ide/src/display.rs:1231`

What: `field_types: &'db ArenaMap<LocalFieldId, FieldType>` becomes
`field_types: &'db hir_ty::FieldTypes` (after request 2), dropping imports that become unused.
Why: `db.field_types(..)` returns `&FieldTypes` (request 1). The function only indexes it.

> applied: `crates/hir-ide/src/display.rs` `render_variant_after_name` takes `&'db FieldTypes` (through hir-ide's `pub use hir_ty::*`); the `la_arena::ArenaMap` and `FieldType` imports are gone. Compiles once 1 and 2 land.

## 4. Coordinator (hir-ide): `crates/hir-ide/src/tests/incremental.rs` query names

What: replace `"GenericPredicates::query_with_diagnostics_"` (9 places) with
`"generic_predicates_of"`, `"impl_trait_with_diagnostics"` (line 638) with
`"impl_trait_of_impl"`, `"impl_self_ty_with_diagnostics"` (line 641) with
`"impl_self_ty_of_impl"`.
Why: the per-item tracked queries were renamed when `GenericPredicates::query_with_diagnostics`,
`impl_trait_with_diagnostics` and `impl_self_ty_with_diagnostics` became plain functions that
choose between a crate batch and the per-item query; the per-item ones are the names the test
log shows for a workspace crate. (The kind table in `db.rs` already uses the new names.)

> applied: `crates/hir-ide/src/tests/incremental.rs`: the nine `GenericPredicates::query_with_diagnostics_` are `generic_predicates_of`, and `impl_trait_of_impl`, `impl_self_ty_of_impl`, as `db.rs`'s kind table names them.

## 5. db-ty-hir: fixture tests in `crates/hir/src/tests.rs`

What: three tests against the `TestDB` pattern there (fixture `//- /dep.rs crate:dep library`
makes a crate whose origin is not local, so its module-level items are batched):

1. `a_dependency_batch_is_what_the_per_item_lowering_gives`: the same declarations in `dep`
   (batched) and in `main` (a query per item): a struct of a few fields, a tuple struct, an
   enum with two variants, an inherent impl and a trait impl with a where clause, all generic
   over `T` and naming only std-free types. For each pair of same-named items, the field types
   (`db.field_types(v).values()`, `ty().skip_binder()`), the impl self types, the impl traits and
   `GenericPredicates::query_all(db, i.into())` print the same (their `Debug`, with the crate's
   own ADTs replaced by their names), and `db.executed` shows `crate_field_types`,
   `crate_impl_self_tys`, `crate_impl_traits` and `crate_impl_predicates` each ran once, for
   `dep`, and `field_types_of_variant`, `impl_self_ty_of_impl`, `impl_trait_of_impl` only for
   `main`'s items.
2. `a_cold_batch_round_trips_with_its_types`: for `dep`'s `CrateFieldTypes::of(db, dep)`,
   `CrateImplSelfTys`, `CrateImplTraits`, `CrateImplPredicates`: `ColdBatch::of_*(interner,
   batch)`, `blaze_format::to_bytes`, `blaze_format::Aligned::copy_from`, `ColdBatch::read`,
   then `.field_types(interner)` (and the like) equals the batch (`assert_eq!`, they derive
   `PartialEq`); and the bytes of two writes are equal.
3. `a_workspace_crate_has_no_batch`: `main`'s items never run a `crate_*` query.

Why: my files have unit tests for everything that needs no database (side tables, the cold
form's columns and checks, the diagnostics' words, supertraits' order, the `params_in_repr`
bitset); the type columns need interned types, which need a database, and hir-ty has none.

> forwarded to db: `crates/hir/src/tests.rs` is inside lane db (db-ty-hir).
> checked by db-audit-dbsub-tysmall (22:33): in the tree at `crates/hir/src/tests.rs:776` (`a_dependency_batch_is_what_the_per_item_lowering_gives`), `:817` (`a_cold_batch_round_trips_with_its_types`) and `:855` (`a_workspace_crate_has_no_batch`).

## 6. Items lane: `crates/blaze-items/src/facts.rs:203` hashes supertraits in their order

Not a change I need, a finding: the interface hash of a trait folds `all_super_traits(db,
trait_)` in slice order. That order was an `FxHashSet`'s (addresses, so different between runs);
it is now the source's (the trait, then its supertraits depth first in the order their bounds
are written; not rustc's `supertrait_def_ids` order, which walks a stack), so the hash no longer
changes between runs. If
the hash should also not change when bounds are reordered, sort the per-supertrait hashes there.

> applied: lane items is DONE; nothing to change. A trait's supertrait order is in its interface: rustc lays out a trait object's vtable by it (`prepare_vtable_segments`), so reordering bounds must change the hash, and the slice order (now deterministic) is the right fold.

## From speed-hir-defs-sigs (2026-10-29): test deprecated-safe before unsafe in `is_fn_unsafe_to_call` (`crates/hir-ty/src/utils.rs`)

What: `crates/hir-def/src/signatures.rs` now gives an `unsafe fn` with
`#[rustc_deprecated_safe_2024]` both `FnFlags::UNSAFE` and `FnFlags::DEPRECATED_SAFE_2024`
(it used to give only the second). This is diagnostics-lints-mir-unsafe's request 6 in
`v2/requests/diagnostics.md`. In `is_fn_unsafe_to_call` (`utils.rs` ~L64-84), move the block

```rust
    if data.is_deprecated_safe_2024() {
        if call_edition.at_least_2024() {
            return Unsafety::Unsafe;
        } else {
            return Unsafety::DeprecatedSafe2024;
        }
    }
```

to the top, before `if data.is_unsafe() { return Unsafety::Unsafe; }`.
Why: rustc's `fn_sig` of such a function (std's `env::set_var`, `env::remove_var`) is `unsafe`,
so its body is an unsafe function's body and it doesn't coerce to a safe `fn` pointer. Before 2024,
only a call of it is let through with the `deprecated_safe_2024` lint (`check_unsafety`). With
the flag change alone, `is_unsafe()` answers first and a pre-2024 `set_var(..)` outside `unsafe`
becomes a hard E0133 where rustc only lints. With the reorder, calls behave as before. `lower.rs`
~L3737 (`if data.is_unsafe() { Safety::Unsafe }`) then gives the unsafe `fn_sig` rustc gives, as
it should. Nothing else changes: blaze-diagnostics' `missing_unsafe.rs:397` already ORs both,
and `hir-ide`'s `unsafe_check.rs:41` now checks the body as an unsafe function's, as rustc does.
Status: open

> applied by db-ty-lower: the deprecated-safe check now comes first in `is_fn_unsafe_to_call`
> (`utils.rs`), before `is_unsafe()` and the target-feature check. `lower.rs`'s `fn_sig` is left
> as it is: it already answers from `is_unsafe()`, so it gives the unsafe signature once
> hir-def sets the flag.
