# Requests from solver-sync-predicate

Owner of `crates/hir-ty/src/next_solver/{predicate.rs,persist.rs}`. Collected by
`solver-sync-callers` / `solver-sync`.

## 1. Fold the type table's codec version into the buffers that hold one

`next_solver::persist::FORMAT_VERSION` (now 2: `TyKind::Alias(IsRigid, _)`, `FnDef`'s binder,
`ConstKind::Alias`, the split alias kinds, no `AliasRelate`) is part of
`persist::TABLE_SCHEMA`, but the buffers that hold a type table are opened under their own
schema, which does not include it. A table written by the 0.166 codec would be read by the new
one (cache key missing an input). Make each buffer schema include `TABLE_SCHEMA`:

- `crates/hir-ty/src/lower/persist.rs:51`
  - old: `const TY_TABLE_SCHEMA: SchemaHash = schema("hir_ty::lower::persist::TyTable");`
  - new: `const TY_TABLE_SCHEMA: SchemaHash = schema_of(&[Part::Str("hir_ty::lower::persist::TyTable"), Part::Schema(crate::next_solver::persist::TABLE_SCHEMA)]);`
    (add `Part` to the `blaze_format` import; `schema_of` is already imported; drop `schema` if
    unused).
- `crates/hir-ty/src/infer/persist.rs:70`: the same, with `"hir_ty::infer::persist::TyTable"`.

`consteval.rs` writes its types into the caller's table, so these two cover it.

> forwarded to db: `crates/hir-ty/src/{lower,infer}/persist.rs` are its.

> applied by db (db-ty-port-ty; checked by db-next2, 2026-10-29 22:16): `lower/persist.rs:50`
> and `infer/persist.rs:79` are `schema_of(&[Part::Str(..), .., Part::Schema(next_solver::persist::TABLE_SCHEMA)])`,
> infer's with `Part::U64(2)` for its `splatted()` encoding. Not compiled.

## 2. `infer/persist.rs`'s closure signature drops `FnSigKind`'s splatted argument

The nightly's `FnSigKind` has a second field, the splatted argument (`splatted() ->
Option<u8>`, `u8::MAX` for none), and `FnSigKind::new(abi, safety, c_variadic, splatted,
args_len) -> Result<_, SplattedArgIndexError>`. `next_solver/persist.rs` now writes it (after
the C-variadic flag) and checks it against the signature's inputs. `infer/persist.rs` must too:

- `crates/hir-ty/src/infer/persist.rs:1596-1601` (writer): add the splatted index in bits 8..16:
  `| u32::from(kind.splatted().unwrap_or(FnSigKind::NO_SPLATTED_ARG_INDEX)) << 8`.
- `:2106-2111` (reader): `if flags >> 16 != 0 || ...`; `let splatted = (flags >> 8) as u8;
  let splatted = (splatted != FnSigKind::NO_SPLATTED_ARG_INDEX).then_some(splatted);`
  `let inputs = inputs_and_output.len().saturating_sub(1);`
  `FnSigKind::new(abi, safety, c_variadic, splatted, inputs).map_err(|_| self.invalid("a splatted argument past the inputs"))?`
  (use the length of the `Tys` just read; `.as_ref()` gives the slice).
- `:2389`, `:2467` (tests) and every other `FnSigKind::new(abi, safety, c_variadic)`
  (`lower.rs:1030`, `:3735`, `:4098`; `interner.rs` is done): the two new arguments
  (`None`, the input count) and `.expect(..)`/`?` on the result, or `FnSigKind::default()
  .set_abi(..).set_safety(..).set_c_variadic(..)` where no splat is possible.

> forwarded to db.

> applied by db (db-ty-port-ty; checked by db-next2, 2026-10-29 22:28): writer `| u32::from(kind.splatted().unwrap_or(NO_SPLATTED_ARG_INDEX)) << 8` (`infer/persist.rs:2273`); reader refuses `flags >> 16 != 0`, a signature with no output, and an index past the inputs (`:3131-3147`); `TY_TABLE_SCHEMA` has `Part::U64(2)`. Tests: the round trip's closures with `Some(1)` and `None`. No three-argument `FnSigKind::new` left. Not compiled.
