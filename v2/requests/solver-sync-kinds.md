# Requests from solver-sync-kinds

Owner of `crates/hir-ty/src/next_solver.rs` and `next_solver/{region.rs, consts.rs, consts/,
allocation.rs, ty.rs, generic_arg.rs, binder.rs, fold.rs, ir_print.rs}`. Collected by
`solver-sync-callers` / `solver-sync`. Report: `/tmp/blaze-lanes/v2-solver-sync-kinds.md`.

What these files now export, for the callers below:

- `Region<'db>` / `Const<'db>` are `rustc_type_ir::Region` / `rustc_type_ir::Const` over the
  handles `InternedRegion<'db>` / `InternedConst<'db>`. Their constructors and predicates
  (`new_param`, `new_bound`, `new_var`, `new_alias`, `new_error`, `new_value`, `from_bool`,
  `from_target_usize`, `try_to_target_usize(interner)`, `is_ct_error`, `new_late_param`,
  `new_static`, ...) are rustc_type_ir's inherent methods. hir-ty's extra ones are in
  `RegionExt` (`new`, `new_without_interner`, `new_erased`, `error`, `index`, `from_index`,
  `store`) and `ConstExt` (`new_without_interner`, `error`, `is_error`, `new_valtree`,
  `new_from_allocation`, `index`, `from_index`, `store`); import them beside the type.
- `UnevaluatedConst` and `ConstKind::Unevaluated` are gone: `ConstKind::Alias(IsRigid,
  AliasConst)` with `AliasConstKind::{Projection, InherentSelf, InherentImpl, Free, Anon}`.
- `TyKind::Alias(IsRigid, AliasTy)`; `TyKind::FnDef(CallableIdWrapper, Binder<GenericArgs>)`.
  The binder is always empty: build with `Ty::new_fn_def(interner, def, args: GenericArgs)`
  (hir-ty's inherent one wraps `Binder::dummy`), read with `args.skip_binder()`.
- `IsRigid`, `ProjectionAliasTy`, `InherentAliasTy`, `OpaqueAliasTy`, `FreeAliasTy` are
  re-exported from `next_solver`.
- `fold::{BoundVarReplacer, FnMutDelegate}` are aliases of rustc_type_ir's
  (`BoundVarReplacer<'db, D>`, `FnMutDelegate<'db, 'a>`), `fold::BoundVarReplacerDelegate` is
  rustc_type_ir's trait, generic over the interner.
- `ValueConst::new(ty, ValTree)` (rustc's order and type); `ValTree::zst()`.

## 1. `crates/hir-ty/src/next_solver/infer/canonical/instantiate.rs:441`: opaque types take `IsRigid`

- old: `.eq(Ty::new_opaque(self.interner, a.def_id.0, a.args), b)?`
- new: `.eq(Ty::new_opaque(self.interner, IsRigid::No, a.def_id.0, a.args), b)?`

Lowering and every place that builds an alias from source makes `IsRigid::No`; only the
solver, having failed to normalize an alias, makes `IsRigid::Yes`. The same holds for every
`Ty::new_alias` / `Ty::new_projection(_from_args)` / `Const::new_alias` call: they now take
`is_rigid` as their second argument.

> forwarded to solver-sync: `next_solver/` is its wholly (db decision 10).
> forwarded to solver-sync: `next_solver/` is its (granted at 20:00, db.md 10).

## 2. `crates/hir-ty/src/infer/pat.rs:1669`: `Const::try_to_target_usize` takes the interner

rustc_type_ir's `Const::try_to_target_usize(self, interner)` is an inherent method now and
shadows the old one that took a data layout.

- old: `if let Some(len) = len.try_to_target_usize(self.data_layout()) {`
- new: `if let Some(len) = len.try_to_value().and_then(|v| v.try_to_target_usize(self.data_layout())) {`

(`ValueConst::try_to_target_usize(&TargetDataLayout)` is kept; `self.data_layout()` is the
crate's layout, which is what inference must use.)

> forwarded to db.
> forwarded to db: `hir-ty/src/infer/` is its (told by tmux).
> applied by db (db-next2, 2026-10-29 22:28): as asked, `infer/pat.rs` `check_array_pat_len`: `len.try_to_value().and_then(|it| it.try_to_target_usize(data_layout))` with the crate's layout (`interner.data_layout()`, which panics rather than defaulting). The interner form the file had read `target_data_layout_or_default`, a fallback: its last caller, `next_solver/consts/valtree.rs:333`, is asked of solver-sync (`v2/requests/solver-sync.md`). Not compiled.

## 3. `crates/hir-ty/src/layout.rs:1101`: `ValueConst::new` takes a `ValTree`

- old: `ConstKind::Value(ValueConst::new(u8, ValTreeKind::Leaf(ScalarInt::from(7u8))))`
- new: `ConstKind::Value(ValueConst::new(u8, ValTree::new(ValTreeKind::Leaf(ScalarInt::from(7u8)))))`

> forwarded to items: `hir-ty/src/layout.rs` is its.
> applied (items): as given, with `ValTree` added to the test module's `next_solver` imports.
> forwarded to items: `hir-ty/src/layout.rs` is its, and its window is live (told by tmux).

## 4. `FnSigKind::new` takes the splatted argument and returns a `Result`

`FnSigKind::new(abi, safety, c_variadic, splatted: Option<u8>, args_len) -> Result<_,
SplattedArgIndexError>`. With no splatted argument it cannot fail; build it with the setters
instead, so no `expect` is needed:

- `crates/hir-ty/src/lower.rs:1030`, `:3735`:
  - old: `FnSigKind::new(abi, safety, varargs)`
  - new: `FnSigKind::default().set_abi(abi).set_safety(safety).set_c_variadic(varargs)`
    (the `// FIXME(splat)` at 1033 stays: hir-def doesn't lower `#[splat]` yet).
- `crates/hir-ty/src/lower.rs:4098`: old `FnSigKind::new(ExternAbi::Rust, Safety::Safe, false)`,
  new `FnSigKind::dummy()`.
- `crates/hir-ty/src/infer/persist.rs:2111`: the codec must store the splatted index too (one
  byte, `FnSigKind::splatted()`), and read it back with
  `FnSigKind::new(abi, safety, c_variadic, splatted, inputs_len)` mapping the error to
  `self.invalid(..)`, as `next_solver/persist.rs:926` does. Its tests at `:2389`, `:2467`:
  `FnSigKind::default().set_abi(..).set_safety(..).set_c_variadic(..)`.
- `crates/hir-ty/src/next_solver/interner.rs:2737` (solver-sync-interner, told by tmux).

`StoredFnSig` (binder.rs) now keeps the splatted index.

> forwarded to db (`lower.rs`, `infer/persist.rs`); the `interner.rs` site is solver-sync-interner's (told by solver-sync-kinds).
> forwarded to db (`lower.rs`, `infer/persist.rs`) and solver-sync (`next_solver/interner.rs`); db told by tmux.
> checked by db (db-next2, 22:28): in the tree. `lower.rs:4120` is `FnSigKind::new(.., data.splatted, data.params.len())` (the `#[splat]` index is lowered now), the others are `FnSigKind::dummy()`/setters; `infer/persist.rs` stores and checks the index (see solver-sync-predicate 2). No three-argument `FnSigKind::new` is left in db's files.

## 5. `StoredEarlyBinder::bind` takes the interner and makes aliases non-rigid

Changed 2026-10-29 19:20 (this replaces the earlier text, which asked each producer to call
`set_aliases_to_non_rigid` itself). Instantiating an early binder needs normalizing again,
which `IsRigid::Yes` would skip, so, like rustc's `EarlyBinder::bind(cx, value)`
(`vendor/rustc_type_ir/src/binder.rs:348`), hir-ty's now does it:

- `StoredEarlyBinder::bind(interner, value)` with `value: TypeFoldable<DbInterner>`; it runs
  `set_aliases_to_non_rigid`, which returns at once, by the value's flags, when nothing is
  rigid (every lowered or decoded value).
- `StoredEarlyBinder::bind_assoc_ty(interner, assoc_ty, args)` for the
  `(TypeAliasId, StoredGenericArgs)` binders (the id can't be folded).
- `StoredEarlyBinder::bind_no_rigid_aliases(value)` where no interner can be had and the value
  has no rigid alias by construction (debug assert), as rustc's `EarlyBinder` has.
- `get`/`get_with` are unchanged (debug assert that nothing is rigid).

So `bind` does the fold itself: no producer (`opaques.rs` included) calls
`set_aliases_to_non_rigid`. Every call site, with its old/new code and owner, is in 8; every
bound type is foldable (listed there).

> forwarded to db and codegen (the call sites: section 8).
> checked by db (db-next2, 22:28): every `StoredEarlyBinder::bind` in db's files (hir-ty but next_solver and items' `opaques.rs`, hir, borrowck, wfcheck) takes the interner. Items' `opaques.rs:159,200,230` still call it with one argument: items'.

## 6. `BoundVarReplacerDelegate` is generic over the interner

- `crates/hir-ty/src/next_solver/infer/mod.rs:1129`:
  - old: `impl<'db> BoundVarReplacerDelegate<'db> for ToFreshVars<'db> {`
  - new: `impl<'db> BoundVarReplacerDelegate<DbInterner<'db>> for ToFreshVars<'db> {`
- `crates/hir-ty/src/next_solver/interner.rs:2284` (solver-sync-interner, told by tmux).

`FnMutDelegate { regions, types, consts }` and `BoundVarReplacer::new(interner, delegate)` keep
their shape (`infer/relate/higher_ranked.rs:39`, `infer/canonical/instantiate.rs:93`,
`util.rs:468`). The replacer now caches folded types (rustc's `DelayedMap`), so a delegate must
give the same answer for the same bound variable, as the trait already asked.

> forwarded to solver-sync: both sites are in `next_solver/`.
> forwarded to solver-sync: both sites are in `next_solver/`.

## 7. `TypeFolder::fold_predicate` / `TypeVisitor::visit_predicate` are generic

Any folder or visitor outside my files that overrides them:

- old: `fn fold_predicate(&mut self, p: Predicate<'db>) -> Predicate<'db>`
- new: `fn fold_predicate<P: PredicateProxy<DbInterner<'db>>>(&mut self, p: P) -> P`

and the same for `visit_predicate` (`-> Self::Result`). `rg -n "fn (fold|visit)_predicate"`
outside `next_solver/{ty,fold}.rs` finds them.

> forwarded to db (`lower.rs:3138`, `infer/unify.rs:792`, `:875`) and solver-sync (`next_solver/infer/canonical/instantiate.rs:171`); none in `requests`' paths.
> forwarded to each implementor's owner (db and solver-sync; none in `hir-ide`, `ide-db` or `test-fixture`).
> checked by db (db-next2, 22:28): `lower.rs:3492`, `infer/unify.rs:846`, `:945` are `fn fold_predicate<P: PredicateProxy<DbInterner<'db>>>(&mut self, p: P) -> P`. No `visit_predicate` override in db's files.

## 8. Every `StoredEarlyBinder::bind` call site (added 2026-10-29 19:30)

Owners (solver-sync, 19:45): "solver-sync-callers" below only means that child traced the site.
The code is lane db's (`crates/hir-ty` outside `next_solver/`, window `db-next2`), except
`crates/hir-ty-mir` (codegen, window `codegen-next1`) and `next_solver/util.rs`
(solver-sync-interner, done).

`StoredEarlyBinder::bind(value)` is now `StoredEarlyBinder::bind(interner, value)` (see 5). At
each site below:

- old: `StoredEarlyBinder::bind(<value>)`
- new: `StoredEarlyBinder::bind(<interner>, <value>)`

with `<interner>` as listed: `interner` is a binding already in scope; `ctx.interner` is the
`TyLoweringContext`'s public field (read it before `ctx` moves into `from_ctx`, which argument
order already does); `DbInterner::new_no_crate(db)` where only `db` is in scope (at
`lower.rs:2214`, `:2278` and `batch.rs:333`, which already build one for `Ty::new_error`, bind
it to `let interner` first and use it for both).

- `crates/hir-ty/src/lower.rs:865`: `interner` — solver-sync-callers
- `crates/hir-ty/src/lower.rs:1660`: `interner` — solver-sync-callers
- `crates/hir-ty/src/lower.rs:1920`: `ctx.interner` — solver-sync-callers
- `crates/hir-ty/src/lower.rs:2077`: `ctx.interner` — solver-sync-callers
- `crates/hir-ty/src/lower.rs:2108`: `ctx.interner` — solver-sync-callers
- `crates/hir-ty/src/lower.rs:2177`: `interner` — solver-sync-callers
- `crates/hir-ty/src/lower.rs:2194`: `interner` — solver-sync-callers
- `crates/hir-ty/src/lower.rs:2214`: `DbInterner::new_no_crate(db)` — solver-sync-callers
- `crates/hir-ty/src/lower.rs:2270`: `ctx.interner` — solver-sync-callers
- `crates/hir-ty/src/lower.rs:2278`: `DbInterner::new_no_crate(db)` — solver-sync-callers
- `crates/hir-ty/src/lower.rs:2507`: `ctx.interner` — solver-sync-callers
- `crates/hir-ty/src/lower.rs:2508`: `ctx.interner` — solver-sync-callers
- `crates/hir-ty/src/lower.rs:2903`: `interner` — solver-sync-callers
- `crates/hir-ty/src/lower.rs:3398`: `interner` — solver-sync-callers
- `crates/hir-ty/src/lower.rs:3519`: `interner` — solver-sync-callers
- `crates/hir-ty/src/lower.rs:3654`: `ctx.interner` — solver-sync-callers
- `crates/hir-ty/src/lower.rs:3732`: `interner` — solver-sync-callers
- `crates/hir-ty/src/lower.rs:4097`: `DbInterner::new_no_crate(db)` — solver-sync-callers
- `crates/hir-ty/src/lower/batch.rs:333`: `DbInterner::new_no_crate(db)` — solver-sync-callers
- `crates/hir-ty/src/lower/batch.rs:373`: `DbInterner::new_no_crate(db)` — solver-sync-callers
- `crates/hir-ty/src/lower/persist.rs:585`: `interner` — solver-sync-callers
- `crates/hir-ty/src/lower/persist.rs:588`: `interner` — solver-sync-callers
- `crates/hir-ty/src/lower/persist.rs:610`: `interner` — solver-sync-callers
- `crates/hir-ty/src/lower/persist.rs:633`: `interner` — solver-sync-callers
- `crates/hir-ty/src/lower/persist.rs:663`: `interner` — solver-sync-callers
- `crates/hir-ty/src/consteval.rs:388`: `interner` — solver-sync-callers
- `crates/hir-ty/src/opaques.rs:151`: `DbInterner::new_no_crate(db)` — solver-sync-callers
- `crates/hir-ty/src/opaques.rs:193`: `interner` — solver-sync-callers
- `crates/hir-ty/src/opaques.rs:214`: `interner` — solver-sync-callers
- `crates/hir-ty/src/opaques.rs:222`: `interner` — solver-sync-callers
- `crates/hir-ty/src/builtin_derive.rs:206`: `interner` — solver-sync-callers
- `crates/hir-ty/src/builtin_derive.rs:228`: `interner` — solver-sync-callers
- `crates/hir-ty/src/builtin_derive.rs:337`: `interner` — solver-sync-callers
- `crates/hir-ty-mir/src/mir/lower.rs:4405`: `DbInterner::new_no_crate(db)` — solver-sync-callers
- `crates/hir-ty/src/next_solver/util.rs:553`, `:577`: `interner` — solver-sync-interner (told).

Different constructor:

- `crates/hir-ty/src/lower.rs:2692`, `:2761` — solver-sync-callers:
  - old: `StoredEarlyBinder::bind((assoc_type, args.store()))`
  - new: `StoredEarlyBinder::bind_assoc_ty(interner, assoc_type, args.store())`
- No interner exists at these three; their values have no rigid alias by construction (empty
  clauses, or clauses read out of an `EarlyBinder`), so they use
  `StoredEarlyBinder::bind_no_rigid_aliases(value)` (rustc's `EarlyBinder::bind_no_rigid_aliases`,
  debug assert) — solver-sync-callers:
  - `crates/hir-ty/src/lower.rs:3200` (`generic_predicates_cycle_result`, `_db`):
    old `StoredEarlyBinder::bind(Clauses::empty().store())`,
    new `StoredEarlyBinder::bind_no_rigid_aliases(Clauses::empty().store())`.
  - `crates/hir-ty/src/lower.rs:3209` (`GenericPredicates::empty`, a `'static` with no db):
    old `StoredEarlyBinder::bind(Clauses::default().store())`,
    new `StoredEarlyBinder::bind_no_rigid_aliases(Clauses::default().store())`.
  - `crates/hir-ty/src/builtin_derive.rs:246` (`explicit_own_predicates`, takes an
    `EarlyBinder`): old `StoredEarlyBinder::bind(`, new `StoredEarlyBinder::bind_no_rigid_aliases(`.

Every type bound here is `TypeFoldable`/`TypeVisitable`: `StoredTy`, `StoredGenericArg`,
`StoredConst` (`impl_foldable_for_stored_type!`), `StoredClauses` (`predicate.rs:283`),
`StoredPolyFnSig` (binder.rs, by hand), `StoredTraitRef` (binder.rs, `#[derive(TypeVisitable,
TypeFoldable)]`); the `(TypeAliasId, StoredGenericArgs)` pair goes through `bind_assoc_ty`.

> forwarded to db (`crates/hir-ty` outside `next_solver/`) and codegen (`crates/hir-ty-mir`), as it says.
> checked by db (db-next2, 2026-10-29 22:31): a scan of every `StoredEarlyBinder::bind(` in hir-ty, hir, borrowck and hir-ty-mir, multi-line calls included, finds one-argument calls only in items' `opaques.rs:159,200,221,230` (asked in `v2/requests/items.md`) and codegen's `hir-ty-mir/src/mir/lower.rs:4652` (asked in `v2/requests/codegen.md`). db's sites are all ported.
