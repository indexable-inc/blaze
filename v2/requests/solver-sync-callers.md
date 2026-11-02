# solver-sync-callers: callers outside next_solver that must move to the nightly's rustc_type_ir

From `solver-sync-callers`, a child of `solver-sync`. The nightly is nightly-2026-10-27 (rustc
`16d1ffac`). The new API is `vendor/rustc_type_ir` and `vendor/rustc_next_trait_solver` in this
tree. The old API (ra-ap 0.166.0) was the base the port started from. The `crates/hir-ty/src/next_solver/` side
belongs to the siblings `solver-sync-interner`, `solver-sync-kinds-next1` and
`solver-sync-predicate`, so it is not listed here. Where a name below is theirs (a re-export, a helper), it is
the name they told me or that is on disk now.

This list was made at HEAD `9bca8dbc32` plus the working tree. Lanes are editing these files, so line
numbers drift. Each site gives its old line, so search for that. Each site appears once, with the
rules that apply (R1 to R15 below). Where the new code is one line, it is given in full. Where a
match arm or a body changes, the new code is given as a block or spelled out in the note.

**Crates.** A search for `rustc_type_ir`, `rustc_next_trait_solver`, `GenericTypeVisitable` and
`next_solver::` outside `crates/hir-ty/src/next_solver/` finds these crates: blaze-diagnostics,
blaze-items, hir, hir-ide, hir-ty, hir-ty-borrowck, hir-ty-codegen, hir-ty-mir, hir-ty-share,
hir-ty-ub, hir-ty-wfcheck and macros. It also finds stdx, but only in a doc comment, so stdx needs
no change. blaze-fmt uses neither crate, so it needs no change. blaze-wasm only has
`ra-ap-rustc_type_ir_macros` entries in its own `Cargo.lock`, which is the coordinator's.

**Owners** come from `docs/v2/plan/reference/crates.md`:

- `db`: hir-ty (except items' files), hir, hir-ty-borrowck and hir-ty-wfcheck.
- `items`: blaze-items, plus these files in hir-ty/src: method_resolution, traits, specialization,
  opaques.rs, layout and drop.
- `codegen`: hir-ty-codegen and hir-ty-mir.
- `diagnostics`: blaze-diagnostics.
- `safety`: hir-ty-ub.
- `fastcompile-ident-share`: hir-ty-share.
- `coordinator`: hir-ide and macros.

## Rules

**R1. Clause renames** (mechanical). The following predicate names are now clause names. Debug output
changes with them: it now prints `TraitClause(..)`.

- `TraitPredicate` → `TraitClause`
- `ProjectionPredicate` → `ProjectionClause`
- `HostEffectPredicate` → `HostEffectClause`
- `OutlivesPredicate<T>` → `OutlivesClause<T>`
- `RegionOutlivesPredicate` / `TypeOutlivesPredicate` / `ArgOutlivesPredicate` → `…Clause`
- Each `Poly…Predicate` form → `Poly…Clause`
- `PredicatePolarity` → `ClausePolarity` (it keeps `flip()`)
- `explicit_super_predicates_of` / `explicit_implied_predicates_of` → `explicit_super_clauses_of` /
  `explicit_implied_clauses_of`

These names are unchanged: `SubtypePredicate`, `CoercePredicate` and their Poly forms,
`ExistentialPredicate`, `ClauseKind`, `PredicateKind`, `NormalizesTo`, `TraitRef` and `AliasTerm`.
`with_self_ty` / `erase_self_ty` now use `ProjectionClause`, so only sites that name that type
change.

**R2. `TyKind::Alias(IsRigid, AliasTy)`.** A pattern `TyKind::Alias(x)` becomes
`TyKind::Alias(_, x)`. A pattern `TyKind::Alias(..)` is unchanged. An arm that rebuilds the type binds
the flag (`TyKind::Alias(is_rigid, x)`) and passes it back.

**R3. Which `IsRigid` a new alias gets.** Use `IsRigid::No` for every alias built outside the
solver: lowering, inference, MIR building, display and tests. In rustc only the solver marks an
alias `Yes`: `instantiate_normalizes_to_as_rigid` (normalizes_to.rs:150) does it when normalization
fails, and `normalize.rs:113,164` then skip `Yes` aliases unless `renormalize_rigid_aliases()`.

- A `Yes` built anywhere else makes the normalizer skip an alias that was never tried, which gives
  wrong types.
- A fold that takes an alias apart and rebuilds it keeps the flag it matched.
- When a type normalized under one TypingMode or ParamEnv is reused under another, call
  `set_aliases_to_non_rigid` (fold.rs) on it first. This is rustc's rule; `EarlyBinder::bind`
  does it for you (R5).

The constructors that now take the flag:

- `Ty::new_alias(i, is_rigid, alias)`
- `Ty::new_projection(i, is_rigid, def, args)` and `new_projection_from_args`
- `AliasTy::to_ty(i, is_rigid)`
- `AliasTerm::to_term(i, is_rigid)`
- `Const::new_alias(i, is_rigid, alias_const)`

- hir-ty's own `Ty::new_opaque(interner, is_rigid, def_id, args)` (kinds confirmed this). It has no
  caller outside next_solver.

All the alias names are reachable as `crate::next_solver::{AliasConst, AliasConstKind, IsRigid,
ProjectionAliasTy, InherentAliasTy, OpaqueAliasTy, FreeAliasTy}` (`hir_ty::next_solver::..` from
other crates). Import them from there, not from `rustc_type_ir`.

**R4. `FnDef(FunctionId, Binder<GenericArgs>)`.** The binder is real: `flags.rs:538`
(`add_binder_args`) computes the args' flags one binder deeper.

- **Build** with `Binder::dummy(args)`: `Ty::new_fn_def(i, def, Binder::dummy(args))`
  (inherent.rs:127 takes the binder) or `TyKind::FnDef(def, Binder::dummy(args))`.
- **Read** with `args.no_bound_vars().unwrap()`. This is what rustc does everywhere it reads them:
  ty_kind.rs:373 (`fn_sig`), structural_traits.rs:288 and 693, walk.rs:142, fast_reject.rs:435 and
  outlives.rs:88. Even rustc's type walker unwraps, so "a FnDef's args never hold bound vars" is a
  global invariant, and the unwrap stops loudly if a bug breaks it. Never use `skip_binder()` to
  read, because it would hide that bug. Write `let args = args.no_bound_vars().unwrap();` once,
  where the pattern binds them.
- **Rebuild** in a fold or relate by passing the binder on: either `args.rebind(new_args)`
  (relate.rs:505-512) or just the folded binder.
- **Visitors that track depth by hand** (a `DebruijnIndex` field): `super_visit_with` now enters a
  binder for the args, so a bound var inside them is one level deeper than before.
- Patterns `FnDef(def, _)` and `FnDef(..)` are unchanged and are not listed below.

**R5. `EarlyBinder::bind(cx, value)`.** binder.rs:348 calls `set_aliases_to_non_rigid`, because
the value will be instantiated in another environment.

- Pass the interner in scope. Where there is none (hir's `Type { ty: EarlyBinder::bind(ty) }`), use
  `DbInterner::new_no_crate(db)`.
- The check is one flag test when nothing is rigid, so there is no reason to use
  `bind_no_rigid_aliases`.
- hir-ty's own `StoredEarlyBinder::bind` now follows the same rule (kinds confirmed this). It is
  `StoredEarlyBinder::bind(interner, value)` and applies `set_aliases_to_non_rigid` itself. The
  `(TypeAliasId, StoredGenericArgs)` binders use `StoredEarlyBinder::bind_assoc_ty(interner,
  assoc_ty, args)`. Its 41 call sites are listed once, in `v2/requests/solver-sync-kinds.md` items 5
  and 8, not here; use `DbInterner::new_no_crate(db)` where no interner is at hand. Because of
  this, infer/opaques.rs needs no explicit `set_aliases_to_non_rigid`.

**R6. TypingMode.** The variants are renamed:

- `Analysis { .. }` → `Typeck { .. }`
- `Borrowck { .. }` → `PostTypeckThroughBorrowck { .. }`
- `PostBorrowckAnalysis { .. }` → `PostBorrowck { .. }`

The constructor functions (`non_body_analysis`, `typeck_for_body`, `analysis_in_body`,
`borrowck`, `post_borrowck_analysis`) are unchanged. There is a new variant, `Codegen`. rustc's
doc says "During codegen and MIR optimizations, we're able to reveal all opaque types and compute
all layouts". In the solver it differs from `PostAnalysis` only in `unstable_feature_bound`, which
holds under `Codegen` (infer_ctxt.rs:611-634: "we may be monomorphizing a body from an upstream
crate").

- Codegen and monomorphization use `Codegen`: hir-ty-codegen mod.rs, v2/places.rs,
  debuginfo/types.rs and hir-ty-mir monomorphization.rs.
- Const eval, layout, analysis and display keep `PostAnalysis`, as rustc's `TypingEnv::post_analysis`
  does.
- `TypingMode::Reflection` now exists too. hir-ty-mir eval/shim/reflection.rs:~662 has a message
  saying the solver lacks it; codegen should make that site use `TypingMode::Reflection`.

**R7. Alias consts.** `ConstKind::Unevaluated(UnevaluatedConst { def, args })` is now
`ConstKind::Alias(IsRigid, AliasConst)`. `AliasConst = Alias<I, AliasConstKind>`, with fields
`kind` and `args` (const_kind.rs:107-145). The kinds:

```rust
AliasConstKind::Projection   { def_id: TraitAssocConstId }    // <T as Trait>::C
AliasConstKind::InherentSelf { def_id: InherentAssocConstId } // HIR lowering: Self form
AliasConstKind::InherentImpl { def_id: InherentAssocConstId } // impl form
AliasConstKind::Free         { def_id: FreeConstAliasId }     // a const outside any impl
AliasConstKind::Anon         { def_id: AnonConstIdWrapper }   // `{ N + 1 }`
```

The three `ConstId` wrappers (`TraitAssocConstId`, `InherentAssocConstId`, `FreeConstAliasId`) hold
the `ConstId` in `.0`. `AnonConstIdWrapper.0` is `crate::db::AnonConstId<'db>` (interner's
def_id.rs). `GeneralConstIdWrapper` is deleted, not renamed: a static never becomes an alias const.
So every `GeneralConstId::StaticId` arm under a const goes, and a path to a static used as a const
is the "not a const" error.

**Reading** an alias const is one match:

```rust
match alias.kind {
    AliasConstKind::Projection { def_id: TraitAssocConstId(id) }
    | AliasConstKind::InherentImpl { def_id: InherentAssocConstId(id) }
    | AliasConstKind::Free { def_id: FreeConstAliasId(id) } => /* the old ConstId arm, args = alias.args */,
    AliasConstKind::Anon { def_id } => /* the old AnonConstId arm, with def_id.0 */,
    AliasConstKind::InherentSelf { .. } => /* stop: error naming the const */,
}
```

`InherentSelf` is how HIR lowering writes `Self::C` in an inherent impl. Its args are
`[Self, own args..]`, and the solver resolves it to the impl form (const_kind.rs:112-134). A
post-typeck MIR constant never holds one. A site that would meet one stops with an error naming the
const: this is no-fallback, so it never guesses impl args. Display and printing may show it like the
other kinds.

**Building** one: `Const::new_alias(i, IsRigid::No, AliasConst::new(i, kind, args))`.
`AliasConst::new` debug-checks the args. Use `from_checked_parts` only where there is no interner
(rows, persisted tables, tests). `Const::new_unevaluated` is gone. To pick the kind for a `ConstId`,
I ask `db` (it owns consteval.rs) for one helper that the MIR builders and `consteval` call:

```rust
/// The alias kind of a MIR constant naming `konst` with its own (impl-form) args. A const in a
/// trait impl has none: MIR names the trait's const (a projection) and `lookup_impl_const`
/// resolves it at eval time, so meeting one here is a bug.
pub fn mir_const_alias_kind<'db>(db: &'db dyn HirDatabase, konst: ConstId) -> AliasConstKind<DbInterner<'db>> {
    match konst.lookup(db).container {
        ItemContainerId::TraitId(_) => AliasConstKind::Projection { def_id: TraitAssocConstId(konst) },
        ItemContainerId::ImplId(impl_) if ImplSignature::of(db, impl_).target_trait.is_none() => {
            AliasConstKind::InherentImpl { def_id: InherentAssocConstId(konst) }
        }
        ItemContainerId::ImplId(_) => panic!("{konst:?} is a trait impl's const: name the trait's"),
        ItemContainerId::ModuleId(_) | ItemContainerId::ExternBlockId(_) => {
            AliasConstKind::Free { def_id: FreeConstAliasId(konst) }
        }
    }
}
```

`AliasTermKind` changes the same way (term_kind.rs):

- `UnevaluatedConst { def_id }` → `AnonConst { def_id: AnonConstIdWrapper }`
- `InherentConst` splits into `InherentConstSelf` and `InherentConstImpl`

An identity or hash encoding needs a tag for each of the two new variants, and must encode
`IsRigid`, because rustc's `StableHash` of `TyKind`/`ConstKind` includes it.

Open design question (to `solver-sync`, not decided here): rustc keeps MIR constants
(`mir::Const::Unevaluated`) apart from type-level consts. hir-ty puts both in `ty::Const`, which is
why MIR needs the mapping above. Splitting them is the long-term rustc design. The mapping above
keeps today's behavior for the port.

**R8. `AliasTyKind::def_id()` is gone.** So are `AliasTermKind::def_id`,
`AliasTerm::{def_id, new_from_def_id, from_unevaluated_const}`, `NormalizesTo::def_id` and
`AliasTyKind::new_from_def_id`. rustc writes a match on the kind. In the listed sites:

- Where the arm above already matched `AliasTyKind::Opaque { .. }` (or `Projection { .. }`), bind
  `def_id` in that pattern and use `def_id.into()` where a `SolverDefId` is needed.
- `a.kind.def_id() == b.kind.def_id()` becomes `a.kind == b.kind`. `AliasTyKind` is `PartialEq`, and
  each variant holds one def id.
- `let SolverDefId::InternedOpaqueTyId(o) = alias.kind.def_id() else { .. }` becomes
  `let AliasTyKind::Opaque { def_id: o } = alias.kind else { .. }`.
- `Alias::try_to_projection` / `try_to_opaque` / `try_to_inherent` / `try_to_free` give an alias
  with the typed kind when that reads better.

`AliasTerm::expect_ty()` and `expect_ct()` no longer take an interner. `expect_ct` returns an
`AliasConst`.

**R9. Folder and visitor overrides are generic.**

```rust
fn fold_predicate<P: PredicateProxy<DbInterner<'db>>>(&mut self, p: P) -> P
fn try_fold_predicate<P: PredicateProxy<DbInterner<'db>>>(&mut self, p: P) -> Result<P, Self::Error>
fn visit_predicate<P: PredicateProxy<DbInterner<'db>>>(&mut self, p: P) -> Self::Result
```

Clauses now go through `fold_predicate` too. `Clauses` lists go through
`fold_clauses` / `visit_clauses`. Bodies that only call `super_fold_with`, `has_type_flags` or
`has_bound_vars` are unchanged.

**R10. Region and Const methods that moved to hir-ty traits.**

- `Region<'db>` is now `rustc_type_ir::Region`, so hir-ty's extra methods live on
  `next_solver::RegionExt`: `new(i, kind)`, `new_without_interner`, `new_erased`, `error`, `index`,
  `from_index` and `store`.
- `Const`'s extra methods live on `next_solver::ConstExt` (the final list, from kinds):
  `new_without_interner`, `error`, `is_error`, `new_valtree`, `new_from_allocation`, `index`,
  `from_index` and `store`. It has no `new`, because `Const::new(interner, kind)` is inherent in
  rustc_type_ir.
- The remaining constructors and `is_*` methods are inherent in rustc_type_ir (sty/mod.rs,
  sty/consts.rs), so they need no import.
- A file that calls one of the moved methods and does not glob-import `next_solver::*` needs the
  trait imported. Files with a static call (`Region::error(..)` and the like) are listed under
  their owner. Method calls (`r.store()`, `c.is_error()`, `r.index()`) cannot be found by search;
  the compiler names them, and the fix is the same import.
- `Region::new_late_param(i, scope, kind)` keeps its shape: `LateParamRegionKind<'db>` is
  `BoundRegion<'db>`, so the `BoundRegion { .. }` the callers pass is still right.
- `try_to_target_usize` now takes the interner, not a `&TargetDataLayout`.

**R11. `GenericTypeVisitable` is an `unsafe trait`** (generic_visit.rs:28: "A manual implementation
**must visit** every field"). A manual impl becomes `unsafe impl`, with a `// SAFETY:` line saying
which fields it visits. The `macros` derive must emit the impl through `s.unsafe_bound_impl(..)` and
wrap it in `#[allow(unsafe_code)]`, as `vendor/rustc_type_ir_macros/src/lib.rs:429-445` does. Its
comment there:

```rust
// SAFETY (of the generated impl): `body_visit` visits every field of every variant, the
// trait's contract, so a crate denying `unsafe_code` may still derive it.
```

**R12. `FnSigKind::new(abi, safety, c_variadic, splatted: Option<u8>, args_len) -> Result<_,
SplattedArgIndexError>`.** `FnSigKind::default` is private now. `FnSigKind::dummy()` gives a safe
`extern "Rust"` signature, and the builders are `set_abi`, `set_safety`, `set_c_variadic` and
`set_splatted(splatted, args_len)`.

- A fn item's signature must carry the `#[splat]` index (rustc `FnDecl::splatted`) from the item
  tree, so lowering writes `FnSigKind::new(abi, safety, variadic, splatted, params.len())`. An
  invalid index is a user error and is reported, never dropped.
- A fn pointer type has no `#[splat]`: `FnSigKind::dummy().set_abi(abi).set_safety(s).set_c_variadic(v)`.
- If the item tree cannot give the index yet, that is a gap to file under superset-of-rust, not a
  quiet `None`.
- `infer/persist.rs` encodes the flags in one word (6 bits of ABI, a safe bit, a variadic bit). It
  must also round-trip `splatted()` and bump its format version. Otherwise a splatted signature
  reads back unsplatted.

**R13. `ParamEnv::caller_bounds()` returns an iterator.** Drop `.iter()`, and write `.is_empty()` as
`.next().is_none()`.

**R14. `try_to_target_usize(self, interner)`** (sty/consts.rs).

**R15. `PredicateKind::AliasRelate` is gone.** Delete its arms.

Unchanged and not listed:

- `InferCtxt::{resolve_vars_if_possible, enter_forall, opportunistic_resolve_*}` and
  `has_opaques_with_sub_unified_hidden_type` are inherent on hir-ty's `InferCtxt`, so their callers
  are unaffected.
- `rustc_next_trait_solver::placeholder::BoundVarReplacer::replace_bound_vars` (used in hir-ty-mir
  monomorphization.rs) keeps its signature.
- No caller outside next_solver calls a removed `Interner` or `InferCtxtLike` method directly.
> applied: the rules the sections below follow; nothing to change by itself.
> applied: the rules the sections below follow; nothing to apply alone.

## db

> checked by db (db-next2, 2026-10-29 22:36), the regenerated list against the code, line by line:
> - 259 sites: the old text is gone from the file.
> - 18 sites are already marked withdrawn (already ported). The `TyKind::Alias` ones that
>   still matched the old text are ported multi-line calls, two fields
>   (`dyn_compatibility.rs:242`, `:495`, `lower.rs:3293`).
> - The R11 `GenericTypeVisitable` impls (`lib.rs:211`, `:383`, `mir.rs:229`) are
>   `unsafe impl` with SAFETY lines.
> - No `ConstKind::Unevaluated` or `Const::new_unevaluated` is left outside `next_solver/`.
> - Every R10 `top` import is present except `hir/src/lib.rs`'s, now added.
> - One suggestion is wrong and not applied: `mir.rs` L142 (see its section).
> Not compiled.

Window `db-next2`. 295 sites in 32 files.


Besides the sites below:

- Add `mir_const_alias_kind` (R7) to `hir-ty/src/consteval.rs`. codegen calls it from hir-ty-mir
  `mir/lower.rs` and `mir/optimize/check_pointers.rs`.
- Fn item lowering (`lower.rs`, the `FnSigKind::new` sites) needs the `#[splat]` index from the
  item tree. `infer/persist.rs` must round-trip `splatted()` (R12).
- `infer/opaques.rs`: `TypingMode::Analysis { defining_opaque_types_and_generators }` becomes
  `TypingMode::Typeck { .. }` (R6). This file is `infer/opaques.rs`, which is db's; items owns
  `hir-ty/src/opaques.rs`.
- Type-table schemas (from solver-sync-predicate, `v2/requests/solver-sync-predicate.md`):
  `lower/persist.rs` and `infer/persist.rs` each have
  `const TY_TABLE_SCHEMA: SchemaHash = schema("hir_ty::…::persist::TyTable");`. That name alone does
  not change when the next_solver codec does, so an old table would read back as the new codec.
  Fold the codec's schema in:

  ```rust
  const TY_TABLE_SCHEMA: SchemaHash = schema_of(&[
      Part::Str("hir_ty::infer::persist::TyTable"), // or ::lower::
      Part::Schema(crate::next_solver::persist::TABLE_SCHEMA),
  ]);
  ```

  In `infer/persist.rs`, add `Part::U64(n)` too, and bump `n` when the closure-signature encoding
  gains `splatted()` (R12). These are the only two `TY_TABLE_SCHEMA`s outside next_solver.
- Every `EarlyBinder::bind(x)` in hir's `Type { ty: .. }` constructors gets
  `DbInterner::new_no_crate(db)` (R5).
> forwarded to db (told by tmux; regenerated list, check each line against the code first).
> forwarded to db (told by solver-sync-callers; told again by requests).
> applied by db-ty-port-ty: `mir_const_alias_kind`, the `#[splat]` index (lower.rs, waiting on hir-def's `FunctionSignature::splatted`, requested in `v2/requests/db-ty.md`), `splatted()` in infer/persist.rs, `TypingMode::Typeck` in infer/opaques.rs, both `TY_TABLE_SCHEMA`s. hir's `Type` item is hir's owner's.
> checked by db (db-next2, 2026-10-29 22:12): every `###` file of this section has an applied
> line, and hir's `Type` item is in too (every `EarlyBinder::bind` in `crates/hir/src` takes the
> interner). Searching db's paths (hir, hir-ty but next_solver and items' files, borrowck,
> wfcheck, matchck, unsafeck) finds none of the old forms: no `OutlivesPredicate`, no
> one-field `TyKind::Alias(..)`, no `ConstKind::Unevaluated`. Still open: the `#[splat]`
> index waits on hir-def's `FunctionSignature::splatted` (`v2/requests/db-ty.md`). Not
> compiled.
> checked by db (db-next2, 22:16), solver-sync-predicate's two items, both in the tree:
> - `lower/persist.rs:50` and `infer/persist.rs:79` build `TY_TABLE_SCHEMA` with `schema_of`,
>   folding in `next_solver::persist::TABLE_SCHEMA`; infer's also has `Part::U64(2)`.
> - `infer/persist.rs` writes `splatted()` in bits 8-15 of the signature's flags
>   (`NO_SPLATTED_ARG_INDEX` for none). The read refuses an index past the inputs (`:3147`).
>   The round-trip test's closures cover both `Some(1)` and `None`.

### `crates/hir-ty-borrowck/src/check.rs`

- L10: `ClauseKind, OutlivesPredicate, RegionKind,` → `ClauseKind, OutlivesClause, RegionKind,`
- L1889: `TyKind::Alias(_) => "associated type",` → `TyKind::Alias(_, _) => "associated type",`
- L2041: `if let TyKind::Alias(alias) = ty.kind() {` → `if let TyKind::Alias(_, alias) = ty.kind() {`
- L2104: `for clause in rustc_type_ir::Interner::item_bounds(interner, alias.kind.def_id())` → `for clause in rustc_type_ir::Interner::item_bounds(interner, opaque_def_id(alias.kind))`. R8: the kind is not matched nearby. If it is an opaque by construction (these are `item_bounds`/`opaque_captures` of opaques), `let AliasTyKind::Opaque { def_id } = alias.kind else { unreachable!("..") };` and pass `def_id.into()`; `opaque_def_id` above stands for that.
- L2107: `if let ClauseKind::TypeOutlives(OutlivesPredicate(bounded, region)) =` → `if let ClauseKind::TypeOutlives(OutlivesClause(bounded, region)) =`
- L2125: `regions::opaque_captures(self.ctx.db, alias.kind.def_id(), alias.args.len())` → `regions::opaque_captures(self.ctx.db, def_id.into(), alias.args.len())`. R8: bind `def_id` in the `AliasTyKind::Opaque { .. }` pattern just above (`Opaque { def_id }`).
- L2144: `TyKind::Alias(inner) => self.alias_test_holds(it, inner, lower, depth + 1),` → `TyKind::Alias(_, inner) => self.alias_test_holds(it, inner, lower, depth + 1),`
> forwarded to db (told by tmux; regenerated list, check each line against the code first).
> applied by db-ty-port-rest

### `crates/hir-ty-borrowck/src/lib.rs`

- L494: `&& let ConstKind::Unevaluated(unevaluated) = konst.as_ref().kind()`

  ```rust
  && let ConstKind::Alias(_, unevaluated) = konst.as_ref().kind()
  && let AliasConstKind::Anon { def_id: AnonConstIdWrapper(id) } = unevaluated.kind
  ```

  R7: replaces this line and the next (`GeneralConstId::AnonConstId(id) = unevaluated.def.0`).
> forwarded to db (told by tmux; regenerated list, check each line against the code first).
> applied by db-ty-port-rest

### `crates/hir-ty-borrowck/src/liveness.rs`

- top: `(imports)` → `use hir_ty::next_solver::{RegionExt};  // or crate::next_solver inside hir-ty`. R10: this file calls a method that moved to the trait.
- L9: `ClauseKind, OutlivesPredicate, RegionKind,` → `ClauseKind, OutlivesClause, RegionKind,`
- L616: `let region = rustc_type_ir::EarlyBinder::bind(region.as_ref())` → `let region = rustc_type_ir::EarlyBinder::bind(interner, region.as_ref())`. R5
- L622: `let whole = rustc_type_ir::EarlyBinder::bind(whole.as_ref())` → `let whole = rustc_type_ir::EarlyBinder::bind(interner, whole.as_ref())`. R5
- L628: `rustc_type_ir::EarlyBinder::bind(it.as_ref())` → `rustc_type_ir::EarlyBinder::bind(interner, it.as_ref())`. R5
- L645: `TyKind::Alias(alias) => {` → `TyKind::Alias(_, alias) => {`
- L649: `.filter(|it| !matches!(it.kind(), TyKind::Alias(_) | TyKind::Infer(_)))` → `.filter(|it| !matches!(it.kind(), TyKind::Alias(_, _) | TyKind::Infer(_)))`
- L744: `rustc_type_ir::EarlyBinder::bind(ty.as_ref())` → `rustc_type_ir::EarlyBinder::bind(interner, ty.as_ref())`. R5
- L749: `rustc_type_ir::EarlyBinder::bind(it.as_ref())` → `rustc_type_ir::EarlyBinder::bind(interner, it.as_ref())`. R5
- L766: `| TyKind::Alias(_)` → `| TyKind::Alias(_, _)`
- L821: `if let TyKind::Alias(alias) = ty.kind() {` → `if let TyKind::Alias(_, alias) = ty.kind() {`
- L834: `alias.kind.def_id(),` → `def_id.into(),`. R8: bind `def_id` in the `AliasTyKind::Opaque { .. }` pattern just above (`Opaque { def_id }`).
- L881: `rustc_type_ir::Interner::item_bounds(interner, alias.kind.def_id())` → `rustc_type_ir::Interner::item_bounds(interner, opaque_def_id(alias.kind))`. R8: the kind is not matched nearby. If it is an opaque by construction (these are `item_bounds`/`opaque_captures` of opaques), `let AliasTyKind::Opaque { def_id } = alias.kind else { unreachable!("..") };` and pass `def_id.into()`; `opaque_def_id` above stands for that.
- L886: `for clause in item_bounds.into_iter().chain(ctx.env.caller_bounds().iter()) {` → `for clause in item_bounds.into_iter().chain(ctx.env.caller_bounds()) {`. R13
- L887: `let ClauseKind::TypeOutlives(OutlivesPredicate(bounded, region)) =` → `let ClauseKind::TypeOutlives(OutlivesClause(bounded, region)) =`
> forwarded to db (told by tmux; regenerated list, check each line against the code first).
> applied by db-ty-port-rest

### `crates/hir-ty-borrowck/src/polonius.rs`

- L271: `TyKind::Alias(alias) => {` → `TyKind::Alias(_, alias) => {`
- L274: `super::regions::opaque_captures(ctx.db, alias.kind.def_id(), alias.args.len())` → `super::regions::opaque_captures(ctx.db, def_id.into(), alias.args.len())`. R8: bind `def_id` in the `AliasTyKind::Opaque { .. }` pattern just above (`Opaque { def_id }`).
- L298: `| TyKind::FnDef(_, args) => {` → `| TyKind::FnDef(_, args) => {`. R4: pattern unchanged; read `args` as `args.no_bound_vars().unwrap()` (once, `let args = args.no_bound_vars().unwrap();`), or pass it on unchanged where it is put back into a `FnDef`.
> forwarded to db (told by tmux; regenerated list, check each line against the code first).
> applied by db-ty-port-rest

### `crates/hir-ty-borrowck/src/regions.rs`

- top: `(imports)` → `use hir_ty::next_solver::{RegionExt};  // or crate::next_solver inside hir-ty`. R10: this file calls a method that moved to the trait.
- L20: `ClauseKind, OutlivesPredicate, RegionKind, Variance,` → `ClauseKind, OutlivesClause, RegionKind, Variance,`
- L256: `if let TyKind::Alias(alias) = ty.kind()` → `if let TyKind::Alias(_, alias) = ty.kind()`
- L259: `self.0.push((alias.kind.def_id(), alias.args));` → `self.0.push((def_id.into(), alias.args));`. R8: bind `def_id` in the `AliasTyKind::Projection { .. }` pattern just above.
- L327: `TyKind::Alias(alias) if alias.kind.def_id() == def => Ty::new_unit(interner),` → `TyKind::Alias(_, alias) if SolverDefId::from(match alias.kind { AliasTyKind::Opaque { def_id } => def_id.into(), AliasTyKind::Projection { def_id } => def_id.into(), AliasTyKind::Inherent { def_id } => def_id.into(), AliasTyKind::Free { def_id } => def_id.into() }) == def => Ty::new_unit(interner),`. R8: `def` is a `SolverDefId`; if it is always an opaque here (it comes from `opaque_captures`), match `AliasTyKind::Opaque { def_id } if SolverDefId::from(def_id) == def` instead, which is shorter and what rustc writes.
- L328: `TyKind::Alias(alias)` → `TyKind::Alias(_, alias)`
- L331: `opaque_captures(db, alias.kind.def_id(), alias.args.len()) =>` → `opaque_captures(db, def_id.into(), alias.args.len()) =>`. R8: bind `def_id` in the `AliasTyKind::Opaque { .. }` pattern just above (`Opaque { def_id }`).
- L509: `if let TyKind::Alias(alias) = ty.kind()` → `if let TyKind::Alias(_, alias) = ty.kind()`
- L536: `(TyKind::Alias(alias), _) if let rustc_type_ir::AliasTyKind::Opaque { .. } = alias.kind => {` → `(TyKind::Alias(_, alias), _) if let rustc_type_ir::AliasTyKind::Opaque { .. } = alias.kind => {`
- L580: `TyKind::Alias(alias)` → `TyKind::Alias(_, alias)`
- L583: `opaque_captures(self.db, alias.kind.def_id(), alias.args.len()) =>` → `opaque_captures(self.db, def_id.into(), alias.args.len()) =>`. R8: bind `def_id` in the `AliasTyKind::Opaque { .. }` pattern just above (`Opaque { def_id }`).
- L608: `let TyKind::Alias(alias) = ty.kind() else { return false };` → `let TyKind::Alias(_, alias) = ty.kind() else { return false };`
- L612: `rustc_type_ir::Interner::item_bounds(interner, alias.kind.def_id())` → `rustc_type_ir::Interner::item_bounds(interner, def_id.into())`. R8: bind `def_id` in the `AliasTyKind::Opaque { .. }` pattern just above (`Opaque { def_id }`).
- L661: `TyKind::Alias(_) if !self.aliases => (),` → `TyKind::Alias(_, _) if !self.aliases => (),`
- L662: `TyKind::Alias(alias)` → `TyKind::Alias(_, alias)`
- L665: `opaque_captures(self.db, alias.kind.def_id(), alias.args.len()) =>` → `opaque_captures(self.db, def_id.into(), alias.args.len()) =>`. R8: bind `def_id` in the `AliasTyKind::Opaque { .. }` pattern just above (`Opaque { def_id }`).
- L1309: `if let TyKind::Alias(alias) = output.kind()` → `if let TyKind::Alias(_, alias) = output.kind()`
- L1335: `let TyKind::Alias(alias) = output.kind() else { return false };` → `let TyKind::Alias(_, alias) = output.kind() else { return false };`
- L1337: `opaque_captures(self.ctx.db, alias.kind.def_id(), alias.args.len())` → `opaque_captures(self.ctx.db, opaque_def_id(alias.kind), alias.args.len())`. R8: the kind is not matched nearby. If it is an opaque by construction (these are `item_bounds`/`opaque_captures` of opaques), `let AliasTyKind::Opaque { def_id } = alias.kind else { unreachable!("..") };` and pass `def_id.into()`; `opaque_def_id` above stands for that.
- L1357: `TyKind::Alias(alias)` → `TyKind::Alias(_, alias)`
- L1366: `opaque_captures(self.ctx.db, alias.kind.def_id(), alias.args.len())` → `opaque_captures(self.ctx.db, def_id.into(), alias.args.len())`. R8: bind `def_id` in the `AliasTyKind::Opaque { .. }` pattern just above (`Opaque { def_id }`).
- L1412: `TyKind::Alias(alias)` → `TyKind::Alias(_, alias)`
- L1441: `&& let TyKind::Alias(alias) = output.kind()` → `&& let TyKind::Alias(_, alias) = output.kind()`
- L1445: `let bounds = rustc_type_ir::Interner::item_bounds(interner, alias.kind.def_id());` → `let bounds = rustc_type_ir::Interner::item_bounds(interner, def_id.into());`. R8: bind `def_id` in the `AliasTyKind::Opaque { .. }` pattern just above (`Opaque { def_id }`).
- L1514: `opaque_captures(self.ctx.db, alias.kind.def_id(), alias.args.len())` → `opaque_captures(self.ctx.db, opaque_def_id(alias.kind), alias.args.len())`. R8: the kind is not matched nearby. If it is an opaque by construction (these are `item_bounds`/`opaque_captures` of opaques), `let AliasTyKind::Opaque { def_id } = alias.kind else { unreachable!("..") };` and pass `def_id.into()`; `opaque_def_id` above stands for that.
- L1538: `rustc_type_ir::Interner::item_bounds(self.interner(), alias.kind.def_id());` → `rustc_type_ir::Interner::item_bounds(self.interner(), opaque_def_id(alias.kind));`. R8: the kind is not matched nearby. If it is an opaque by construction (these are `item_bounds`/`opaque_captures` of opaques), `let AliasTyKind::Opaque { def_id } = alias.kind else { unreachable!("..") };` and pass `def_id.into()`; `opaque_def_id` above stands for that.
- L1541: `ClauseKind::TypeOutlives(OutlivesPredicate(bounded, region))` → `ClauseKind::TypeOutlives(OutlivesClause(bounded, region))`
- L1542: `if let TyKind::Alias(bounded) = bounded.kind()` → `if let TyKind::Alias(_, bounded) = bounded.kind()`
- L1543: `&& bounded.kind.def_id() == alias.kind.def_id() =>` → `&& bounded.kind == alias.kind =>`. R8: compare the kinds (each variant holds one def id, so equal kinds is equal def ids).
- L2106: `&& let TyKind::FnDef(def, args) = ty.as_ref().kind()` → `&& let TyKind::FnDef(def, args) = ty.as_ref().kind()`. R4: pattern unchanged; read `args` as `args.no_bound_vars().unwrap()` (once, `let args = args.no_bound_vars().unwrap();`), or pass it on unchanged where it is put back into a `FnDef`.
- L2112: `Some(TyKind::FnDef(_, args)) => Some(args),` → `Some(TyKind::FnDef(_, args)) => Some(args),`. R4: pattern unchanged; read `args` as `args.no_bound_vars().unwrap()` (once, `let args = args.no_bound_vars().unwrap();`), or pass it on unchanged where it is put back into a `FnDef`.
- L2298: `TyKind::Alias(alias) => {` → `TyKind::Alias(_, alias) => {`
- L2322: `TyKind::FnDef(def, args) => {` → `TyKind::FnDef(def, args) => {`. R4: pattern unchanged; read `args` as `args.no_bound_vars().unwrap()` (once, `let args = args.no_bound_vars().unwrap();`), or pass it on unchanged where it is put back into a `FnDef`.
- L2462: `for clause in self.ctx.env.caller_bounds().iter() {` → `for clause in self.ctx.env.caller_bounds() {`. R13
- L2463: `if let ClauseKind::TypeOutlives(OutlivesPredicate(ty, _)) = clause.kind().skip_binder()` → `if let ClauseKind::TypeOutlives(OutlivesClause(ty, _)) = clause.kind().skip_binder()`
- L2464: `&& let TyKind::Alias(_) = ty.kind()` → `&& let TyKind::Alias(_, _) = ty.kind()`
- L2569: `TyKind::Alias(alias)` → `TyKind::Alias(_, alias)`
- L2597: `ClauseKind::TypeOutlives(OutlivesPredicate(ty, _)) => tys.push(ty),` → `ClauseKind::TypeOutlives(OutlivesClause(ty, _)) => tys.push(ty),`
- L2654: `ClauseKind::RegionOutlives(OutlivesPredicate(longer, shorter)) => {` → `ClauseKind::RegionOutlives(OutlivesClause(longer, shorter)) => {`
- L2663: `ClauseKind::TypeOutlives(OutlivesPredicate(ty, region)) => {` → `ClauseKind::TypeOutlives(OutlivesClause(ty, region)) => {`
- L2678: `let clauses: Vec<_> = self.ctx.env.caller_bounds().iter().collect();` → `let clauses: Vec<_> = self.ctx.env.caller_bounds().collect();`. R13
- L2681: `ClauseKind::TypeOutlives(OutlivesPredicate(ty, region)) => {` → `ClauseKind::TypeOutlives(OutlivesClause(ty, region)) => {`
- L2689: `ClauseKind::RegionOutlives(OutlivesPredicate(longer, shorter)) => {` → `ClauseKind::RegionOutlives(OutlivesClause(longer, shorter)) => {`
- L2893: `TyKind::Alias(_) => self.normalize(inner).unwrap_or(inner),` → `TyKind::Alias(_, _) => self.normalize(inner).unwrap_or(inner),`
- L2926: `let OutlivesPredicate(arg, region) =` → `let OutlivesClause(arg, region) =`
- L2927: `rustc_type_ir::EarlyBinder::bind(predicate)` → `rustc_type_ir::EarlyBinder::bind(interner, predicate)`. R5
- L2932: `OutlivesPredicate(ty, region).upcast(interner)` → `OutlivesClause(ty, region).upcast(interner)`
- L2935: `OutlivesPredicate(longer, region).upcast(interner)` → `OutlivesClause(longer, region).upcast(interner)`
- L2993: `TyKind::Alias(alias) => {` → `TyKind::Alias(_, alias) => {`
- L3073: `params.iter().filter(|it| matches!(it.kind(), TyKind::Alias(_))).copied().collect();` → `params.iter().filter(|it| matches!(it.kind(), TyKind::Alias(_, _))).copied().collect();`
- L3089: `ClauseKind::RegionOutlives(OutlivesPredicate(longer, shorter)) => {` → `ClauseKind::RegionOutlives(OutlivesClause(longer, shorter)) => {`
- L3096: `ClauseKind::TypeOutlives(OutlivesPredicate(ty, region)) => {` → `ClauseKind::TypeOutlives(OutlivesClause(ty, region)) => {`
- L3200: `let TyKind::Alias(future) = sig.output().kind() else { return None };` → `let TyKind::Alias(_, future) = sig.output().kind() else { return None };`
- L3202: `.item_bounds(future.kind.def_id())` → `.item_bounds(opaque_def_id(future.kind))`. R8: the kind is not matched nearby. If it is an opaque by construction (these are `item_bounds`/`opaque_captures` of opaques), `let AliasTyKind::Opaque { def_id } = alias.kind else { unreachable!("..") };` and pass `def_id.into()`; `opaque_def_id` above stands for that.
- L3226: `(kind, TyKind::Alias(alias))` → `(kind, TyKind::Alias(_, alias))`
- L3228: `&& !matches!(kind, TyKind::Alias(_)) =>` → `&& !matches!(kind, TyKind::Alias(_, _)) =>`
- L3263: `let TyKind::Alias(alias) = inner.kind() else { return };` → `let TyKind::Alias(_, alias) = inner.kind() else { return };`
- L3265: `let Some(captured) = opaque_captures(self.ctx.db, alias.kind.def_id(), alias.args.len())` → `let Some(captured) = opaque_captures(self.ctx.db, def_id.into(), alias.args.len())`. R8: bind `def_id` in the `AliasTyKind::Opaque { .. }` pattern just above (`Opaque { def_id }`).
- L3274: `.item_bounds(alias.kind.def_id())` → `.item_bounds(opaque_def_id(alias.kind))`. R8: the kind is not matched nearby. If it is an opaque by construction (these are `item_bounds`/`opaque_captures` of opaques), `let AliasTyKind::Opaque { def_id } = alias.kind else { unreachable!("..") };` and pass `def_id.into()`; `opaque_def_id` above stands for that.
- L3278: `ClauseKind::TypeOutlives(OutlivesPredicate(bounded, region))` → `ClauseKind::TypeOutlives(OutlivesClause(bounded, region))`
- L3279: `if let TyKind::Alias(bounded) = bounded.kind()` → `if let TyKind::Alias(_, bounded) = bounded.kind()`
- L3280: `&& bounded.kind.def_id() == alias.kind.def_id() =>` → `&& bounded.kind == alias.kind =>`. R8: compare the kinds (each variant holds one def id, so equal kinds is equal def ids).
- L3616: `(TyKind::Alias(x), TyKind::Alias(y)) if x.kind == y.kind => {` → `(TyKind::Alias(_, x), TyKind::Alias(_, y)) if x.kind == y.kind => {`
- L3638: `opaque_captures(self.ctx.db, x.kind.def_id(), x.args.len()).map(|it| {` → `opaque_captures(self.ctx.db, def_id.into(), x.args.len()).map(|it| {`. R8: bind `def_id` in the `AliasTyKind::Opaque { .. }` pattern just above (`Opaque { def_id }`).
- L3654: `(TyKind::Alias(x), _) | (_, TyKind::Alias(x))` → `(TyKind::Alias(_, x), _) | (_, TyKind::Alias(_, x))`
- L3658: `TyKind::Alias(_) => this.normalize_at(ty, at).filter(|&it| it != ty),` → `TyKind::Alias(_, _) => this.normalize_at(ty, at).filter(|&it| it != ty),`
- L3663: `&& !matches!((a.kind(), b.kind()), (TyKind::Alias(_), TyKind::Alias(_)))` → `&& !matches!((a.kind(), b.kind()), (TyKind::Alias(_, _), TyKind::Alias(_, _)))`
- L3676: `(TyKind::FnDef(def, aa), TyKind::FnDef(_, ab)) => {` → `(TyKind::FnDef(def, aa), TyKind::FnDef(_, ab)) => {`. R4: pattern unchanged; read `aa` as `aa.no_bound_vars().unwrap()` (once, `let aa = aa.no_bound_vars().unwrap();`), or pass it on unchanged where it is put back into a `FnDef`.
- L3754: `let TyKind::Alias(alias) = ty.kind() else { return };` → `let TyKind::Alias(_, alias) = ty.kind() else { return };`
- L3763: `&& !matches!(normalized.kind(), TyKind::Alias(_) | TyKind::Infer(_))` → `&& !matches!(normalized.kind(), TyKind::Alias(_, _) | TyKind::Infer(_))`
- L3776: `rustc_type_ir::Interner::item_bounds(interner, alias.kind.def_id())` → `rustc_type_ir::Interner::item_bounds(interner, opaque_def_id(alias.kind))`. R8: the kind is not matched nearby. If it is an opaque by construction (these are `item_bounds`/`opaque_captures` of opaques), `let AliasTyKind::Opaque { def_id } = alias.kind else { unreachable!("..") };` and pass `def_id.into()`; `opaque_def_id` above stands for that.
- L3779: `ClauseKind::TypeOutlives(OutlivesPredicate(bounded, bound))` → `ClauseKind::TypeOutlives(OutlivesClause(bounded, bound))`
- L3794: `.then(|| opaque_captures(self.ctx.db, alias.kind.def_id(), alias.args.len()))` → `.then(|| opaque_captures(self.ctx.db, opaque_def_id(alias.kind), alias.args.len()))`. R8: the kind is not matched nearby. If it is an opaque by construction (these are `item_bounds`/`opaque_captures` of opaques), `let AliasTyKind::Opaque { def_id } = alias.kind else { unreachable!("..") };` and pass `def_id.into()`; `opaque_def_id` above stands for that.
- L3928: `if let TyKind::FnDef(_, args) = ty.kind() {` → `if let TyKind::FnDef(_, args) = ty.kind() {`. R4: pattern unchanged; read `args` as `args.no_bound_vars().unwrap()` (once, `let args = args.no_bound_vars().unwrap();`), or pass it on unchanged where it is put back into a `FnDef`.
- L3931: `if let TyKind::FnDef(def, args) = ty.kind()` → `if let TyKind::FnDef(def, args) = ty.kind()`. R4: pattern unchanged; read `args` as `args.no_bound_vars().unwrap()` (once, `let args = args.no_bound_vars().unwrap();`), or pass it on unchanged where it is put back into a `FnDef`.
- L3946: `let TyKind::FnDef(def, args) = ty.kind() else { return None };` → `let TyKind::FnDef(def, args) = ty.kind() else { return None };`. R4: pattern unchanged; read `args` as `args.no_bound_vars().unwrap()` (once, `let args = args.no_bound_vars().unwrap();`), or pass it on unchanged where it is put back into a `FnDef`.
- L3954: `Some(Ty::new_fn_def(self.interner(), def, args))` → `Some(Ty::new_fn_def(self.interner(), def, Binder::dummy(args)))`. R4
- L3992: `|| !matches!(ty.kind(), TyKind::Alias(alias) if matches!(alias.kind, rustc_type_ir::AliasTyKind::Projection { .. }))` → `|| !matches!(ty.kind(), TyKind::Alias(_, alias) if matches!(alias.kind, rustc_type_ir::AliasTyKind::Projection { .. }))`
- L4159: `let ConstKind::Unevaluated(unevaluated) = konst.as_ref().kind() else { return None };` → `let ConstKind::Alias(_, unevaluated) = konst.as_ref().kind() else { return None };`. R7
- L4265: `if let TyKind::FnDef(def, args) = ty.kind() {` → `if let TyKind::FnDef(def, args) = ty.kind() {`. R4: pattern unchanged; read `args` as `args.no_bound_vars().unwrap()` (once, `let args = args.no_bound_vars().unwrap();`), or pass it on unchanged where it is put back into a `FnDef`.
- L4288: `if let TyKind::FnDef(def, args) = ty.kind() {` → `if let TyKind::FnDef(def, args) = ty.kind() {`. R4: pattern unchanged; read `args` as `args.no_bound_vars().unwrap()` (once, `let args = args.no_bound_vars().unwrap();`), or pass it on unchanged where it is put back into a `FnDef`.
- L4452: `let TyKind::FnDef(def, inferred) = ty.kind() else { return None };` → `let TyKind::FnDef(def, inferred) = ty.kind() else { return None };`. R4: pattern unchanged; read `inferred` as `inferred.no_bound_vars().unwrap()` (once, `let inferred = inferred.no_bound_vars().unwrap();`), or pass it on unchanged where it is put back into a `FnDef`.
- L4518: `if let TyKind::Alias(alias) = ty.kind()` → `if let TyKind::Alias(_, alias) = ty.kind()`
- L4617: `(TyKind::Alias(alias), _)` → `(TyKind::Alias(_, alias), _)`
- L4774: `if let TyKind::FnDef(def, args) = source.kind() {` → `if let TyKind::FnDef(def, args) = source.kind() {`. R4: pattern unchanged; read `args` as `args.no_bound_vars().unwrap()` (once, `let args = args.no_bound_vars().unwrap();`), or pass it on unchanged where it is put back into a `FnDef`.
- L5009: `TyKind::FnDef(def, args)` → `TyKind::FnDef(def, args)`. R4: pattern unchanged; read `args` as `args.no_bound_vars().unwrap()` (once, `let args = args.no_bound_vars().unwrap();`), or pass it on unchanged where it is put back into a `FnDef`.
- L5401: `TyKind::FnDef(def, fn_args) => {` → `TyKind::FnDef(def, fn_args) => {`. R4: pattern unchanged; read `fn_args` as `fn_args.no_bound_vars().unwrap()` (once, `let fn_args = fn_args.no_bound_vars().unwrap();`), or pass it on unchanged where it is put back into a `FnDef`.
- L5774: `let OutlivesPredicate(arg, region) = rustc_type_ir::EarlyBinder::bind(predicate)` → `let OutlivesClause(arg, region) = rustc_type_ir::EarlyBinder::bind(interner, predicate)`. R1/R2 on this line too. R5
- L5779: `GenericArgKind::Type(ty) => OutlivesPredicate(ty, region).upcast(interner),` → `GenericArgKind::Type(ty) => OutlivesClause(ty, region).upcast(interner),`
- L5781: `OutlivesPredicate(longer, region).upcast(interner)` → `OutlivesClause(longer, region).upcast(interner)`
- L5801: `ClauseKind::RegionOutlives(OutlivesPredicate(longer, shorter)) => {` → `ClauseKind::RegionOutlives(OutlivesClause(longer, shorter)) => {`
- L5812: `ClauseKind::TypeOutlives(OutlivesPredicate(ty, region)) if type_outlives => {` → `ClauseKind::TypeOutlives(OutlivesClause(ty, region)) if type_outlives => {`
- L5852: `&& let Some(alias) = projection.projection_term.to_term(interner).as_type()` → `&& let Some(alias) = projection.projection_term.to_term(interner, IsRigid::No).as_type()`. R3: a projection term from a clause, about to be normalized.
- L5939: `if let ClauseKind::TypeOutlives(OutlivesPredicate(ty, region)) =` → `if let ClauseKind::TypeOutlives(OutlivesClause(ty, region)) =`
- L5988: `if let ClauseKind::TypeOutlives(OutlivesPredicate(bounded, region)) =` → `if let ClauseKind::TypeOutlives(OutlivesClause(bounded, region)) =`
- L5990: `&& let TyKind::Alias(alias) = bounded.kind()` → `&& let TyKind::Alias(_, alias) = bounded.kind()`
- L5991: `&& alias.kind.def_id() == assoc` → `&& opaque_def_id(alias.kind) == assoc`. R8: the kind is not matched nearby. If it is an opaque by construction (these are `item_bounds`/`opaque_captures` of opaques), `let AliasTyKind::Opaque { def_id } = alias.kind else { unreachable!("..") };` and pass `def_id.into()`; `opaque_def_id` above stands for that.
- L6022: `ClauseKind::TypeOutlives(OutlivesPredicate(ty, sub))` → `ClauseKind::TypeOutlives(OutlivesClause(ty, sub))`
- L6030: `ClauseKind::RegionOutlives(OutlivesPredicate(longer, shorter)) => {` → `ClauseKind::RegionOutlives(OutlivesClause(longer, shorter)) => {`
- L6153: `for clause in self.ctx.env.caller_bounds().iter() {` → `for clause in self.ctx.env.caller_bounds() {`. R13
- L6214: `let predicate = rustc_type_ir::TraitPredicate {` → `let predicate = rustc_type_ir::TraitClause {`
- L6216: `polarity: rustc_type_ir::PredicatePolarity::Positive,` → `polarity: rustc_type_ir::ClausePolarity::Positive,`
- L6476: `rustc_type_ir::TraitPredicate {` → `rustc_type_ir::TraitClause {`
- L6478: `polarity: rustc_type_ir::PredicatePolarity::Positive,` → `polarity: rustc_type_ir::ClausePolarity::Positive,`
- L6632: `ClauseKind::TypeOutlives(OutlivesPredicate(ty, region))` → `ClauseKind::TypeOutlives(OutlivesClause(ty, region))`
- L6837: `(TyKind::Alias(alias), _)` → `(TyKind::Alias(_, alias), _)`
- L6858: `matches!(ty.kind(), TyKind::Alias(alias)` → `matches!(ty.kind(), TyKind::Alias(_, alias)`
- L6927: `TyKind::FnDef(_, args) => {` → `TyKind::FnDef(_, args) => {`. R4: pattern unchanged; read `args` as `args.no_bound_vars().unwrap()` (once, `let args = args.no_bound_vars().unwrap();`), or pass it on unchanged where it is put back into a `FnDef`.
- L6963: `TyKind::Alias(alias)` → `TyKind::Alias(_, alias)`
- L6995: `TyKind::Adt(_, it) | TyKind::FnDef(_, it) => args(it, out),` → `TyKind::Adt(_, it) | TyKind::FnDef(_, it) => args(it, out),`. R4: pattern unchanged; read `it` as `it.no_bound_vars().unwrap()` (once, `let it = it.no_bound_vars().unwrap();`), or pass it on unchanged where it is put back into a `FnDef`.
> forwarded to db (told by tmux; regenerated list, check each line against the code first).
> applied by db-ty-port-rest

### `crates/hir-ty-wfcheck/src/lib.rs`

- L410: `&& let TyKind::Alias(_, bounded) = bounded.kind()`. Withdrawn: this line is already ported; my regenerated suggestion was wrong. Nothing to do.
> forwarded to db (told by tmux; regenerated list, check each line against the code first).
> applied by db-ty-port-rest

### `crates/hir-ty-wfcheck/src/outlives.rs`

- L53: `TyKind::Alias(_, _) if !ty.has_escaping_bound_vars() => {`. Withdrawn: this line is already ported; my regenerated suggestion was wrong. Nothing to do.
- L77: `if let TyKind::Alias(_, alias) = ty.kind()`. Withdrawn: this line is already ported; my regenerated suggestion was wrong. Nothing to do.
- L114: `TyKind::Alias(_, alias)`. Withdrawn: this line is already ported; my regenerated suggestion was wrong. Nothing to do.
- L192: `EarlyBinder::bind(interner, predicate).instantiate(interner, args).skip_norm_wip();`. Withdrawn: this line is already ported; my regenerated suggestion was wrong. Nothing to do.
- L293: `TyKind::Alias(_, alias) => args(inferred, alias.args, out),`. Withdrawn: this line is already ported; my regenerated suggestion was wrong. Nothing to do.
- L316: `TyKind::Alias(_, alias)`. Withdrawn: this line is already ported; my regenerated suggestion was wrong. Nothing to do.
- L351: `TyKind::Alias(_, alias)`. Withdrawn: this line is already ported; my regenerated suggestion was wrong. Nothing to do.
- L655: `let OutlivesClause(arg, region) = EarlyBinder::bind(interner, predicate)`. Withdrawn: this line is already ported; my regenerated suggestion was wrong. Nothing to do.
- L665: `TyKind::Alias(_, alias)`. Withdrawn: this line is already ported; my regenerated suggestion was wrong. Nothing to do.
- L827: `let TyKind::Alias(_, alias) = ty.kind() else { return true };`. Withdrawn: this line is already ported; my regenerated suggestion was wrong. Nothing to do.
> forwarded to db (told by tmux; regenerated list, check each line against the code first).
> applied by db-ty-port-rest

### `crates/hir-ty/src/autoderef.rs`

- L362: `Ty::new_projection(interner, IsRigid::No, trait_target.into(), [ty]),`. Withdrawn: this line is already ported; my regenerated suggestion was wrong. Nothing to do.
> forwarded to db (told by tmux; regenerated list, check each line against the code first).
> applied by db-ty-port-ty

### `crates/hir-ty/src/builtin_derive.rs`

- L132: `EarlyBinder::bind(interner, TraitRef::new_from_args(`. Withdrawn: this line is already ported; my regenerated suggestion was wrong. Nothing to do.
- L147: `return EarlyBinder::bind(interner, TraitRef::new(`. Withdrawn: this line is already ported; my regenerated suggestion was wrong. Nothing to do.
- L155: `EarlyBinder::bind(` → `EarlyBinder::bind(interner,`. R5
- L371: `if let TyKind::Alias(_, AliasTy { kind: AliasTyKind::Projection { .. }, .. }) = t.kind() {`. Withdrawn: this line is already ported; my regenerated suggestion was wrong. Nothing to do.
> forwarded to db (told by tmux; regenerated list, check each line against the code first).
> applied by db-ty-port-ty

### `crates/hir-ty/src/consteval.rs`

- top: `(imports)` → `use hir_ty::next_solver::{ConstExt};  // or crate::next_solver inside hir-ty`. R10: this file calls a method that moved to the trait.
- L30: `UnevaluatedConst, ValTreeKind, default_types,` → `ValTreeKind, AliasConst, AliasConstKind, IsRigid,`. R7: `UnevaluatedConst` is gone; the alias-const names come from `crate::next_solver`.
- L215: `unevaluated: UnevaluatedConst<'db>,` → `unevaluated: AliasConst<DbInterner<'db>>,`. R7. The body: `match unevaluated.kind { Projection { def_id: TraitAssocConstId(id) } | InherentImpl { def_id: InherentAssocConstId(id) } | Free { def_id: FreeConstAliasId(id) } => const_eval_query(db, id, unevaluated.args.store(), None), Anon { def_id } => anon_const_eval_query(db, def_id.0, unevaluated.args.store(), None), InherentSelf { .. } => panic!("the inherent const {unevaluated:?} is in Self form after type check") }`. The `StaticId` arm goes: statics are not consts (`const_eval_static_query` stays for its own callers).
- L236: `ConstKind::Unevaluated(unevaluated) => evaluated(db, unevaluated).map(allocation_as_usize),` → `ConstKind::Alias(_, unevaluated) => evaluated(db, unevaluated).map(allocation_as_usize),`. R7
- L259: `ConstKind::Unevaluated(unevaluated) => evaluated(db, unevaluated).map(allocation_as_isize),` → `ConstKind::Alias(_, unevaluated) => evaluated(db, unevaluated).map(allocation_as_isize),`. R7
- L321: `Ok(Const::new_unevaluated(interner, UnevaluatedConst { def: konst.into(), args }))` → `Ok(Const::new_alias(interner, IsRigid::No, AliasConst::new(interner, mir_const_alias_kind(db, konst), args)))`. R7, R3. `konst` here comes from a path (`ValueNs::ConstId`/`ValueNs::StaticId`, consteval.rs:~301): the `StaticId` case now returns the "not a const" error instead of building a const (rustc: a static is not a valid const argument). `mir_const_alias_kind` is the new helper of R7.
- L398: `Ok(Const::new_unevaluated(`

  ```rust
  Ok(Const::new_alias(
      interner,
      IsRigid::No,
      AliasConst::new(interner, AliasConstKind::Anon { def_id: AnonConstIdWrapper(konst) }, args),
  ))
  ```

  R7, R3: the next line's `UnevaluatedConst { def: GeneralConstId::AnonConstId(konst).into(), args }` folds into this.

- L400: `UnevaluatedConst { def: GeneralConstId::AnonConstId(konst).into(), args },` → `(removed: folded into the `Const::new_alias` above)`. R7
> forwarded to db (told by tmux; regenerated list, check each line against the code first).
> applied by db-ty-port-ty

### `crates/hir-ty/src/dyn_compatibility.rs`

- L14: `AliasTyKind, ClauseKind, PredicatePolarity, TypeSuperVisitable as _, TypeVisitable as _,` → `AliasTyKind, ClauseKind, ClausePolarity, TypeSuperVisitable as _, TypeVisitable as _,`
- L25: `SolverDefId, TraitPredicate, TraitRef, Ty, TypingMode, Unnormalized,` → `SolverDefId, TraitClause, TraitRef, Ty, TypingMode, Unnormalized,`
- L244: `rustc_type_ir::TyKind::Alias(` → `rustc_type_ir::TyKind::Alias(_,`
- L378: `if let ClauseKind::Trait(TraitPredicate {` → `if let ClauseKind::Trait(TraitClause {`
- L380: `polarity: PredicatePolarity::Positive,` → `polarity: ClausePolarity::Positive,`
- L488: `EarlyBinder::bind(receiver_ty).instantiate(interner, args).skip_norm_wip()` → `EarlyBinder::bind(interner, receiver_ty).instantiate(interner, args).skip_norm_wip()`. R5
- L504: `if let rustc_type_ir::TyKind::Alias(AliasTy {` → `if let rustc_type_ir::TyKind::Alias(_, AliasTy {`
> forwarded to db (told by tmux; regenerated list, check each line against the code first).
> applied by db-ty-port-ty

### `crates/hir-ty/src/effects.rs`

- L306: `(None, Some(TyKind::FnDef(def, args))) => match def.0 {` → `(None, Some(TyKind::FnDef(def, args))) => match def.0 {`. R4: pattern unchanged; read `args` as `args.no_bound_vars().unwrap()` (once, `let args = args.no_bound_vars().unwrap();`), or pass it on unchanged where it is put back into a `FnDef`.
> forwarded to db (told by tmux; regenerated list, check each line against the code first).
> applied by db-ty-port-ty

### `crates/hir-ty/src/infer.rs`

- top: `(imports)` → `use hir_ty::next_solver::{RegionExt};  // or crate::next_solver inside hir-ty`. R10: this file calls a method that moved to the trait.
- L2293: `&& let ConstKind::Unevaluated(konst) = konst.kind()` → `&& let ConstKind::Alias(_, konst) = konst.kind()`. R7
- L2586: `let alias = Ty::new_alias(` → `let alias = Ty::new_alias(`. R3: add `IsRigid::No,` after the interner argument on the next line.
> forwarded to db (told by tmux; regenerated list, check each line against the code first).
> applied by db-ty-port-ty

### `crates/hir-ty/src/infer/callee.rs`

- L365: `(Some(CallableDefId::FunctionId(func)), TyKind::FnDef(_, fn_generic_args)) => {` → `(Some(CallableDefId::FunctionId(func)), TyKind::FnDef(_, fn_generic_args)) => {`. R4: pattern unchanged; read `fn_generic_args` as `fn_generic_args.no_bound_vars().unwrap()` (once, `let fn_generic_args = fn_generic_args.no_bound_vars().unwrap();`), or pass it on unchanged where it is put back into a `FnDef`.
- L437: `TyKind::FnDef(def_id, args) => {` → `TyKind::FnDef(def_id, args) => {`. R4: pattern unchanged; read `args` as `args.no_bound_vars().unwrap()` (once, `let args = args.no_bound_vars().unwrap();`), or pass it on unchanged where it is put back into a `FnDef`.
> forwarded to db (told by tmux; regenerated list, check each line against the code first).
> applied by db-ty-port-ty

### `crates/hir-ty/src/infer/cast.rs`

- top: `(imports)` → `use hir_ty::next_solver::{RegionExt};  // or crate::next_solver inside hir-ty`. R10: this file calls a method that moved to the trait.
> forwarded to db (told by tmux; regenerated list, check each line against the code first).
> applied by db-ty-port-ty

### `crates/hir-ty/src/infer/closure.rs`

- L28: `PolyProjectionPredicate, Predicate, PredicateKind, SolverDefId, StoredFnSig, TermId, Ty,` → `PolyProjectionClause, Predicate, PredicateKind, SolverDefId, StoredFnSig, TermId, Ty,`
- L376: `TyKind::Alias(AliasTy { kind: rustc_type_ir::Opaque { def_id }, args, .. }) => self` → `TyKind::Alias(_, AliasTy { kind: rustc_type_ir::Opaque { def_id }, args, .. }) => self`
- L568: `projection: PolyProjectionPredicate<'db>,` → `projection: PolyProjectionClause<'db>,`
- L599: `projection: PolyProjectionPredicate<'db>,` → `projection: PolyProjectionClause<'db>,`
- L645: `projection: PolyProjectionPredicate<'db>,` → `projection: PolyProjectionClause<'db>,`
- L1074: `TyKind::Alias(AliasTy { kind: AliasTyKind::Projection { .. }, .. }) => {` → `TyKind::Alias(_, AliasTy { kind: AliasTyKind::Projection { .. }, .. }) => {`
- L1077: `TyKind::Alias(AliasTy { kind: AliasTyKind::Opaque { def_id }, args, .. }) => def_id` → `TyKind::Alias(_, AliasTy { kind: AliasTyKind::Opaque { def_id }, args, .. }) => def_id`
- L1101: `predicate: PolyProjectionPredicate<'db>,` → `predicate: PolyProjectionClause<'db>,`
> forwarded to db (told by tmux; regenerated list, check each line against the code first).
> applied by db-ty-port-ty

### `crates/hir-ty/src/infer/coerce.rs`

- top: `(imports)` → `use hir_ty::next_solver::{RegionExt};  // or crate::next_solver inside hir-ty`. R10: this file calls a method that moved to the trait.
> forwarded to db (told by tmux; regenerated list, check each line against the code first).
> applied by db-ty-port-ty

### `crates/hir-ty/src/infer/expr.rs`

- L804: `TyKind::FnDef(def, parameters) => {` → `TyKind::FnDef(def, parameters) => {`. R4: pattern unchanged; read `parameters` as `parameters.no_bound_vars().unwrap()` (once, `let parameters = parameters.no_bound_vars().unwrap();`), or pass it on unchanged where it is put back into a `FnDef`.
- L958: `Ty::new_projection(self.interner(), into_future_output.into(), [awaitee_ty]),` → `Ty::new_projection(self.interner(), IsRigid::No, into_future_output.into(), [awaitee_ty]),`. R3
- L1782: `if let TyKind::FnDef(def_id, args) = callee_ty.kind() {` → `if let TyKind::FnDef(def_id, args) = callee_ty.kind() {`. R4: pattern unchanged; read `args` as `args.no_bound_vars().unwrap()` (once, `let args = args.no_bound_vars().unwrap();`), or pass it on unchanged where it is put back into a `FnDef`.
- L1882: `Ty::new_fn_def(` → `Ty::new_fn_def(`. R4: the call spans lines; wrap its last argument (the args) in `Binder::dummy(..)`.
> forwarded to db (told by tmux; regenerated list, check each line against the code first).
> applied by db-ty-port-ty

### `crates/hir-ty/src/infer/opaques.rs`

- L108: `let ty = EarlyBinder::bind(interner, ty.ty);`. Withdrawn: this line is already ported; my regenerated suggestion was wrong. Nothing to do.
> forwarded to db (told by tmux; regenerated list, check each line against the code first).
> applied by db-ty-port-ty

### `crates/hir-ty/src/infer/pat.rs`

- L1309: `let target_ty = Ty::new_projection(interner, IsRigid::No, deref_target.into(), [source_ty]);`. Withdrawn: this line is already ported; my regenerated suggestion was wrong. Nothing to do.
- L1669: `if let Some(len) = len.try_to_target_usize(self.interner()) {` → `if let Some(len) = len.try_to_target_usize(self.interner()) {`. R14: takes the interner now (sty/consts.rs).
> forwarded to db (told by tmux; regenerated list, check each line against the code first).
> applied by db-ty-port-ty

### `crates/hir-ty/src/infer/persist.rs`

- L31: `use rustc_type_ir::{ClosureKind, PredicatePolarity};` → `use rustc_type_ir::{ClosureKind, ClausePolarity};`
- L1256: `PredicatePolarity::Positive => 0,` → `ClausePolarity::Positive => 0,`
- L1257: `PredicatePolarity::Negative => 1,` → `ClausePolarity::Negative => 1,`
- L1789: `0 => PredicatePolarity::Positive,` → `0 => ClausePolarity::Positive,`
- L1790: `1 => PredicatePolarity::Negative,` → `1 => ClausePolarity::Negative,`
- L2111: `let fn_sig_kind = FnSigKind::new(ExternAbi::from_packed(abi as u8), safety, flags & (1 << 7) != 0);` → `let fn_sig_kind = FnSigKind::new(ExternAbi::from_packed(abi as u8), safety, flags & (1 << 7) != 0);`. R12
- L2389: `fn_sig_kind: FnSigKind::new(ExternAbi::RustCall, Safety::Safe, false),` → `fn_sig_kind: FnSigKind::new(ExternAbi::RustCall, Safety::Safe, false),`. R12
- L2467: `fn_sig_kind: FnSigKind::new(ExternAbi::C { unwind: true }, Safety::Unsafe, true),` → `fn_sig_kind: FnSigKind::new(ExternAbi::C { unwind: true }, Safety::Unsafe, true),`. R12
> forwarded to db (told by tmux; regenerated list, check each line against the code first).
> applied by db-ty-port-ty

### `crates/hir-ty/src/infer/unify.rs`

- L260: `| PredicateKind::AliasRelate(..)` → `(delete this line)`. R15: `PredicateKind::AliasRelate` is gone.
- L399: `if let ConstKind::Unevaluated(..) = ct.kind() {` → `if let ConstKind::Alias(..) = ct.kind() {`. R7
- L527: `TyKind::Alias(alias)` → `TyKind::Alias(_, alias)`
- L792: `fn fold_predicate(&mut self, p: Predicate<'db>) -> Predicate<'db> {` → `fn fold_predicate<P: PredicateProxy<DbInterner<'db>>>(&mut self, p: P) -> P {`. R9: body unchanged (`has_bound_vars`, `has_type_flags`, `super_fold_with` all work on `P`). Import `rustc_type_ir::PredicateProxy`.
- L875: `fn fold_predicate(&mut self, predicate: Predicate<'db>) -> Predicate<'db> {` → `fn fold_predicate<P: PredicateProxy<DbInterner<'db>>>(&mut self, predicate: P) -> P {`. R9: body unchanged (`has_bound_vars`, `has_type_flags`, `super_fold_with` all work on `P`). Import `rustc_type_ir::PredicateProxy`.
> forwarded to db (told by tmux; regenerated list, check each line against the code first).
> applied by db-ty-port-ty

### `crates/hir-ty/src/lib.rs`

- top: `(imports)` → `use hir_ty::next_solver::{ConstExt, RegionExt};  // or crate::next_solver inside hir-ty`. R10: this file calls a method that moved to the trait.
- L180: `impl<V> rustc_type_ir::GenericTypeVisitable<V> for StaticMemory {`

  ```rust
  // SAFETY: visits every field (it has none that hold types).
  unsafe impl<V> rustc_type_ir::GenericTypeVisitable<V> for StaticMemory {
  ```

  R11: say in the SAFETY line which fields it visits; a manual impl that skips a field that holds a type is unsound now.

- L350: `impl<V> rustc_type_ir::GenericTypeVisitable<V> for MemoryPieces {`

  ```rust
  // SAFETY: visits every field (it has none that hold types).
  unsafe impl<V> rustc_type_ir::GenericTypeVisitable<V> for MemoryPieces {
  ```

  R11: say in the SAFETY line which fields it visits; a manual impl that skips a field that holds a type is unsound now.

- L814: `let output_projection = Ty::new_alias(` → `let output_projection = Ty::new_alias(`. R3: add `IsRigid::No,` after the interner argument on the next line.
> forwarded to db (told by tmux; regenerated list, check each line against the code first).
> applied by db-ty-port-ty

### `crates/hir-ty/src/lower.rs`

- top: `(imports)` → `use hir_ty::next_solver::{RegionExt};  // or crate::next_solver inside hir-ty`. R10: this file calls a method that moved to the trait.
- L50: `Interner, OutlivesPredicate, TermKind, TyKind, TypeFoldable, TypeFolder, TypeSuperFoldable,` → `Interner, OutlivesClause, TermKind, TyKind, TypeFoldable, TypeFolder, TypeSuperFoldable,`
- L78: `StoredGenericArgs, StoredPolyFnSig, StoredTraitRef, StoredTy, TraitPredicate, TraitRef, Ty,` → `StoredGenericArgs, StoredPolyFnSig, StoredTraitRef, StoredTy, TraitClause, TraitRef, Ty,`
- L687: `&& let ConstKind::Unevaluated(konst) = konst.kind()`

  ```rust
  && let ConstKind::Alias(_, konst) = konst.kind()
  && let AliasConstKind::Anon { def_id: AnonConstIdWrapper(konst) } = konst.kind
  ```

  R7: the next line (`GeneralConstId::AnonConstId(konst) = konst.def.0`) is replaced by the second line here.

- L938: `Ty::new_alias(` → `Ty::new_alias(`. R3: add `IsRigid::No,` after the interner argument on the next line.
- L1030: `fn_sig_kind: FnSigKind::new(` → `fn_sig_kind: FnSigKind::new(`. R12
- L1191: `rustc_type_ir::ClauseKind::RegionOutlives(OutlivesPredicate(` → `rustc_type_ir::ClauseKind::RegionOutlives(OutlivesClause(`
- L1240: `TraitPredicate {` → `TraitClause {`
- L1242: `polarity: rustc_type_ir::PredicatePolarity::Positive,` → `polarity: rustc_type_ir::ClausePolarity::Positive,`
- L1305: `rustc_type_ir::ClauseKind::TypeOutlives(OutlivesPredicate(` → `rustc_type_ir::ClauseKind::TypeOutlives(OutlivesClause(`
- L1456: `ClauseKind::TypeOutlives(OutlivesPredicate(ty, outlived))` → `ClauseKind::TypeOutlives(OutlivesClause(ty, outlived))`
- L1616: `let self_ty = Ty::new_alias(` → `let self_ty = Ty::new_alias(`. R3: add `IsRigid::No,` after the interner argument on the next line.
- L1645: `rustc_type_ir::ClauseKind::Trait(TraitPredicate {` → `rustc_type_ir::ClauseKind::Trait(TraitClause {`
- L1647: `polarity: rustc_type_ir::PredicatePolarity::Positive,` → `polarity: rustc_type_ir::ClausePolarity::Positive,`
- L2028: `TyDefId::BuiltinType(it) => EarlyBinder::bind(Ty::from_builtin_type(interner, it)),` → `TyDefId::BuiltinType(it) => EarlyBinder::bind(interner, Ty::from_builtin_type(interner, it)),`. R5
- L2029: `TyDefId::AdtId(it) => EarlyBinder::bind(Ty::new_adt(` → `TyDefId::AdtId(it) => EarlyBinder::bind(interner, Ty::new_adt(`. R5
- L2042: `EarlyBinder::bind(Ty::new_fn_def(` → `EarlyBinder::bind(Ty::new_fn_def(`. R4: the call spans lines; wrap its last argument (the args) in `Binder::dummy(..)`.
- L2124: `Some(EarlyBinder::bind(Ty::new_fn_def(` → `Some(EarlyBinder::bind(Ty::new_fn_def(`. R4: the call spans lines; wrap its last argument (the args) in `Binder::dummy(..)`.
- L2145: `Some(EarlyBinder::bind(Ty::new_fn_def(` → `Some(EarlyBinder::bind(Ty::new_fn_def(`. R4: the call spans lines; wrap its last argument (the args) in `Binder::dummy(..)`.
- L2758: `let args = EarlyBinder::bind(args)` → `let args = EarlyBinder::bind(interner, args)`. R5
- L2850: `let interner_ty = Ty::new_projection_from_args(interner, type_alias.into(), item_args);` → `let interner_ty = Ty::new_projection_from_args(interner, IsRigid::No, type_alias.into(), item_args);`. R3
- L2940: `if let TyKind::Alias(` → `if let TyKind::Alias(_,`
- L3138: `fn fold_predicate(&mut self, p: Predicate<'db>) -> Predicate<'db> {` → `fn fold_predicate<P: PredicateProxy<DbInterner<'db>>>(&mut self, p: P) -> P {`. R9: body unchanged (`has_bound_vars`, `has_type_flags`, `super_fold_with` all work on `P`). Import `rustc_type_ir::PredicateProxy`.
- L3482: `rustc_type_ir::ClauseKind::Trait(TraitPredicate {` → `rustc_type_ir::ClauseKind::Trait(TraitClause {`
- L3484: `polarity: rustc_type_ir::PredicatePolarity::Positive,` → `polarity: rustc_type_ir::ClausePolarity::Positive,`
- L3735: `fn_sig_kind: FnSigKind::new(` → `fn_sig_kind: FnSigKind::new(`. R12
- L3873: `TyKind::Alias(alias)` → `TyKind::Alias(_, alias)`
- L3875: `alias.kind.def_id()` → `(two lines: `if let AliasTyKind::Opaque { def_id: opaque } = alias.kind`)`. R8: replaces `if let crate::next_solver::SolverDefId::InternedOpaqueTyId(opaque) = alias.kind.def_id()`; `opaque` is then the `InternedOpaqueTyId` the next line uses.
- L3888: `Ty::new_alias(` → `Ty::new_alias(`. R3: add `IsRigid::No,` after the interner argument on the next line.
- L3890: `AliasTy::new_from_args(interner, alias.kind, GenericArgs::new_from_iter(args)),` → `AliasTy::new_from_args(interner, alias.kind, GenericArgs::new_from_iter(args)),`. R3: `Ty::new_alias(interner, is_rigid, ..)` on the line above, with `is_rigid` bound in `TyKind::Alias(is_rigid, alias)` at the top of the arm: a fold keeps what it does not change.
- L4084: `EarlyBinder::bind(ty)` → `EarlyBinder::bind(interner, ty)`. R5
- L4098: `fn_sig_kind: FnSigKind::new(ExternAbi::Rust, Safety::Safe, false),` → `fn_sig_kind: FnSigKind::new(ExternAbi::Rust, Safety::Safe, false),`. R12
- L4141: `EarlyBinder::bind(trait_args).instantiate(interner, trait_ref.args).skip_norm_wip(),` → `EarlyBinder::bind(interner, trait_args).instantiate(interner, trait_ref.args).skip_norm_wip(),`. R5
> forwarded to db (told by tmux; regenerated list, check each line against the code first).
> applied by db-ty-port-ty

### `crates/hir-ty/src/lower/path.rs`

- L39: `GenericArgs, Predicate, ProjectionPredicate, Region, TraitRef, Ty,` → `GenericArgs, Predicate, ProjectionClause, Region, TraitRef, Ty,`
- L232: `Ty::new_alias(` → `Ty::new_alias(`. R3: add `IsRigid::No,` after the interner argument on the next line.
- L557: `EarlyBinder::bind(trait_args)` → `EarlyBinder::bind(interner, trait_args)`. R5
- L575: `Ty::new_projection_from_args(interner, assoc_type.into(), substs)` → `Ty::new_projection_from_args(interner, IsRigid::No, assoc_type.into(), substs)`. R3
- L1046: `ProjectionPredicate {` → `ProjectionClause {`
- L1066: `Ty::new_alias(` → `Ty::new_alias(`. R3: add `IsRigid::No,` after the interner argument on the next line.
> forwarded to db (told by tmux; regenerated list, check each line against the code first).
> applied by db-ty-port-ty

### `crates/hir-ty/src/mir.rs`

- L142: `let ty = Ty::new_fn_def(interner, CallableDefId::FunctionId(func_id).into(), generic_args);` → `let ty = Ty::new_fn_def(interner, CallableDefId::FunctionId(func_id).into(), Binder::dummy(generic_args));`. R4
- L227: `impl<W: crate::next_solver::WorldExposer> GenericTypeVisitable<W> for PlaceElem {`

  ```rust
  // SAFETY: visits every field (it has none that hold types).
  unsafe impl<W: crate::next_solver::WorldExposer> GenericTypeVisitable<W> for PlaceElem {
  ```

  R11: say in the SAFETY line which fields it visits; a manual impl that skips a field that holds a type is unsound now.
> forwarded to db (told by tmux; regenerated list, check each line against the code first).
> applied by db-ty-port-ty
> checked by db (db-next2, 2026-10-29 22:36): L227 is `unsafe impl` with its SAFETY line (`mir.rs:229`). L142's suggestion is wrong, not applied: `Ty::new_fn_def(interner, def_id, args: GenericArgs)` wraps the args in `Binder::dummy` itself (`next_solver/ty.rs:235`), so the call stays as it is.

### `crates/hir-ty/src/solver_errors.rs`

- L10: `ClosureKind, Interner, PredicatePolarity,` → `ClosureKind, Interner, ClausePolarity,`
- L18: `ClauseKind, DbInterner, PolyTraitPredicate, PredicateKind, StoredTraitRef, TraitPredicate,` → `ClauseKind, DbInterner, PolyTraitClause, PredicateKind, StoredTraitRef, TraitClause,`
- L55: `pub polarity: PredicatePolarity,` → `pub polarity: ClausePolarity,`
- L60: `pub fn get<'db>(&'db self, interner: DbInterner<'db>) -> TraitPredicate<'db> {` → `pub fn get<'db>(&'db self, interner: DbInterner<'db>) -> TraitClause<'db> {`
- L61: `TraitPredicate { polarity: self.polarity, trait_ref: self.trait_ref.get(interner) }` → `TraitClause { polarity: self.polarity, trait_ref: self.trait_ref.get(interner) }`
- L100: `trait_pred: PolyTraitPredicate<'db>,` → `trait_pred: PolyTraitClause<'db>,`
- L158: `trait_pred: TraitPredicate<'db>,` → `trait_pred: TraitClause<'db>,`
> forwarded to db (told by tmux; regenerated list, check each line against the code first).
> applied by db-ty-port-ty

### `crates/hir-ty/src/variance.rs`

- L257: `TyKind::Alias(_, alias) => {`. Withdrawn: this line is already ported; my regenerated suggestion was wrong. Nothing to do.
> forwarded to db (told by tmux; regenerated list, check each line against the code first).
> applied by db-ty-port-ty

### `crates/hir/src/diagnostics.rs`

- L747: `pub trait_predicate: crate::TraitPredicate<'db>,` → `pub trait_predicate: crate::TraitClause<'db>,`
- L748: `pub parent_trait_predicates: Vec<crate::TraitPredicate<'db>>,` → `pub parent_trait_predicates: Vec<crate::TraitClause<'db>>,`
- L1935: `let new_ty = |ty| Type { owner: type_owner, ty: EarlyBinder::bind(ty) };` → `let new_ty = |ty| Type { owner: type_owner, ty: EarlyBinder::bind(DbInterner::new_no_crate(db), ty) };`. R5: no interner in scope; `DbInterner::new_no_crate(db)` (or the crate-scoped one if `db`/`krate` are at hand).
- L1960: `found: Type { owner: type_owner, ty: EarlyBinder::bind(found.as_ref()) },` → `found: Type { owner: type_owner, ty: EarlyBinder::bind(DbInterner::new_no_crate(db), found.as_ref()) },`. R5: no interner in scope; `DbInterner::new_no_crate(db)` (or the crate-scoped one if `db`/`krate` are at hand).
- L2211: `expected: Type { owner: type_owner, ty: EarlyBinder::bind(expected.as_ref()) },` → `expected: Type { owner: type_owner, ty: EarlyBinder::bind(DbInterner::new_no_crate(db), expected.as_ref()) },`. R5: no interner in scope; `DbInterner::new_no_crate(db)` (or the crate-scoped one if `db`/`krate` are at hand).
- L2212: `actual: Type { owner: type_owner, ty: EarlyBinder::bind(found.as_ref()) },` → `actual: Type { owner: type_owner, ty: EarlyBinder::bind(DbInterner::new_no_crate(db), found.as_ref()) },`. R5: no interner in scope; `DbInterner::new_no_crate(db)` (or the crate-scoped one if `db`/`krate` are at hand).
- L2310: `let trait_predicate = crate::TraitPredicate {` → `let trait_predicate = crate::TraitClause {`
- L2316: `.map(|trait_predicate| crate::TraitPredicate {` → `.map(|trait_predicate| crate::TraitClause {`
> forwarded to db (told by tmux; regenerated list, check each line against the code first).
> applied by db-ty-port-rest

### `crates/hir/src/display.rs`

- L36: `LifetimeParam, Macro, Module, SelfParam, Static, Struct, StructKind, Trait, TraitPredicate,` → `LifetimeParam, Macro, Module, SelfParam, Static, Struct, StructKind, Trait, TraitClause,`
- L901: `impl<'db> HirDisplay<'db> for TraitPredicate<'db> {` → `impl<'db> HirDisplay<'db> for TraitClause<'db> {`
> forwarded to db (told by tmux; regenerated list, check each line against the code first).
> applied by db-ty-port-rest

### `crates/hir/src/lib.rs`

- top: `(imports)` → `use hir_ty::next_solver::{RegionExt};  // or crate::next_solver inside hir-ty`. R10: this file calls a method that moved to the trait.
- L1642: `Type { owner, ty: EarlyBinder::bind(sig.output()) }` → `Type { owner, ty: EarlyBinder::bind(interner, sig.output()) }`. R5
- L1698: `ty: Type { owner, ty: EarlyBinder::bind(ty) },` → `ty: Type { owner, ty: EarlyBinder::bind(interner, ty) },`. R5
- L2085: `Type { owner, ty: EarlyBinder::bind(sig.inputs()[0]) }` → `Type { owner, ty: EarlyBinder::bind(DbInterner::new_no_crate(db), sig.inputs()[0]) }`. R5: no interner in scope; `DbInterner::new_no_crate(db)` (or the crate-scoped one if `db`/`krate` are at hand).
- L3268: `Type { ty: EarlyBinder::bind(ty), owner: self.owner },` → `Type { ty: EarlyBinder::bind(DbInterner::new_no_crate(db), ty), owner: self.owner },`. R5: no interner in scope; `DbInterner::new_no_crate(db)` (or the crate-scoped one if `db`/`krate` are at hand).
- L3282: `.map(|(ty, _)| Type { ty: EarlyBinder::bind(ty), owner: self.owner })` → `.map(|(ty, _)| Type { ty: EarlyBinder::bind(DbInterner::new_no_crate(db), ty), owner: self.owner })`. R5: no interner in scope; `DbInterner::new_no_crate(db)` (or the crate-scoped one if `db`/`krate` are at hand).
- L4098: `Type { owner: self.owner, ty: EarlyBinder::bind(ty) }` → `Type { owner: self.owner, ty: EarlyBinder::bind(DbInterner::new_no_crate(db), ty) }`. R5: no interner in scope; `DbInterner::new_no_crate(db)` (or the crate-scoped one if `db`/`krate` are at hand).
- L4109: `.map(|ty| Type { owner: self.owner, ty: EarlyBinder::bind(ty) })` → `.map(|ty| Type { owner: self.owner, ty: EarlyBinder::bind(DbInterner::new_no_crate(db), ty) })`. R5: no interner in scope; `DbInterner::new_no_crate(db)` (or the crate-scoped one if `db`/`krate` are at hand).
- L4574: `Type { owner: TypeOwnerId::GenericDefId(owner), ty: EarlyBinder::bind(ty) }` → `Type { owner: TypeOwnerId::GenericDefId(owner), ty: EarlyBinder::bind(DbInterner::new_no_crate(db), ty) }`. R5: no interner in scope; `DbInterner::new_no_crate(db)` (or the crate-scoped one if `db`/`krate` are at hand).
- L4582: `Type { owner: TypeOwnerId::NoParams(krate), ty: EarlyBinder::bind(ty) }` → `Type { owner: TypeOwnerId::NoParams(krate), ty: EarlyBinder::bind(DbInterner::new_no_crate(db), ty) }`. R5: no interner in scope; `DbInterner::new_no_crate(db)` (or the crate-scoped one if `db`/`krate` are at hand).
- L4658: `Type { owner, ty: EarlyBinder::bind(self.ty.instantiate(interner, args).skip_norm_wip()) }` → `Type { owner, ty: EarlyBinder::bind(interner, self.ty.instantiate(interner, args).skip_norm_wip()) }`. R5
- L4682: `ty: EarlyBinder::bind(self.ty.instantiate(interner, args).skip_norm_wip()),` → `ty: EarlyBinder::bind(interner, self.ty.instantiate(interner, args).skip_norm_wip()),`. R5
- L4772: `let ty = EarlyBinder::bind(Ty::new_tup_from_iter(` → `let ty = EarlyBinder::bind(interner, Ty::new_tup_from_iter(`. R5
- L5171: `let projection = Ty::new_alias(` → `let projection = Ty::new_alias(`. R3: add `IsRigid::No,` after the interner argument on the next line.
- L5182: `if ty.is_ty_error() { None } else { Some(Type { owner, ty: EarlyBinder::bind(ty) }) }` → `if ty.is_ty_error() { None } else { Some(Type { owner, ty: EarlyBinder::bind(interner, ty) }) }`. R5
- L5826: `let TyKind::Alias(AliasTy { kind: AliasTyKind::Projection { def_id }, .. }) =` → `let TyKind::Alias(_, AliasTy { kind: AliasTyKind::Projection { def_id }, .. }) =`
- L5838: `Type { owner: self.owner, ty: EarlyBinder::bind(ty) }` → `Type { owner: self.owner, ty: EarlyBinder::bind(DbInterner::new_no_crate(db), ty) }`. R5: no interner in scope; `DbInterner::new_no_crate(db)` (or the crate-scoped one if `db`/`krate` are at hand).
- L5861: `(self.callback)(Type { owner: self.owner, ty: EarlyBinder::bind(ty) });` → `(self.callback)(Type { owner: self.owner, ty: EarlyBinder::bind(DbInterner::new_no_crate(db), ty) });`. R5: no interner in scope; `DbInterner::new_no_crate(db)` (or the crate-scoped one if `db`/`krate` are at hand).
- L6297: `pub enum PredicatePolarity {` → `pub enum ClausePolarity {`
- L6305: `pub struct TraitPredicate<'db> {` → `pub struct TraitClause<'db> {`
- L6306: `inner: hir_ide::next_solver::TraitPredicate<'db>,` → `inner: hir_ide::next_solver::TraitClause<'db>,`
- L6310: `impl<'db> TraitPredicate<'db> {` → `impl<'db> TraitClause<'db> {`
- L6311: `pub fn polarity(&self) -> PredicatePolarity {` → `pub fn polarity(&self) -> ClausePolarity {`
- L6313: `rustc_type_ir::PredicatePolarity::Positive => PredicatePolarity::Positive,` → `rustc_type_ir::ClausePolarity::Positive => ClausePolarity::Positive,`
- L6314: `rustc_type_ir::PredicatePolarity::Negative => PredicatePolarity::Negative,` → `rustc_type_ir::ClausePolarity::Negative => ClausePolarity::Negative,`
> forwarded to db (told by tmux; regenerated list, check each line against the code first).
> applied by db-ty-port-rest

> applied by db (db-next2, 2026-10-29 22:36): the `top` import: `crates/hir/src/lib.rs` calls `Region::error(interner)` (`:5151`), which is `RegionExt`'s now (no inherent `error` in rustc_type_ir), so its `next_solver::{..}` import gains `RegionExt as _`.

### `crates/hir/src/source_analyzer.rs`

- L308: `Type { owner: self.type_owner, ty: EarlyBinder::bind(ty) }` → `Type { owner: self.type_owner, ty: EarlyBinder::bind(DbInterner::new_no_crate(db), ty) }`. R5: no interner in scope; `DbInterner::new_no_crate(db)` (or the crate-scoped one if `db`/`krate` are at hand).
- L1184: `if let TyKind::FnDef(_fn_def, subs) = func_ty.kind() {` → `if let TyKind::FnDef(_fn_def, subs) = func_ty.kind() {`. R4: pattern unchanged; read `subs` as `subs.no_bound_vars().unwrap()` (once, `let subs = subs.no_bound_vars().unwrap();`), or pass it on unchanged where it is put back into a `FnDef`.
- L1452: `TyKind::Alias(AliasTy {` → `TyKind::Alias(_, AliasTy {`
- L1463: `TyKind::FnDef(fn_id, subst) => {` → `TyKind::FnDef(fn_id, subst) => {`. R4: pattern unchanged; read `subst` as `subst.no_bound_vars().unwrap()` (once, `let subst = subst.no_bound_vars().unwrap();`), or pass it on unchanged where it is put back into a `FnDef`.
> forwarded to db (told by tmux; regenerated list, check each line against the code first).
> applied by db-ty-port-rest

## items

Window `items`. 9 sites in 5 files.


Besides the sites below:

- In `blaze-items/src/form.rs`, `general_const` becomes `alias_const(f, AliasConstKind)` with one
  tag per kind. The form also encodes `IsRigid` for `TyKind::Alias` and `ConstKind::Alias`, and its
  version is bumped (R7).
> forwarded to items (told by tmux; regenerated list, check each line against the code first).
> checked (items): already applied. `form.rs` has `alias_const` with one tag per kind, writes `IsRigid` after both alias tags (lines 1449 and 1785), and its domain is `"blaze v2 form 2"`.
> forwarded to items (told by solver-sync-callers; told again by requests).

### `crates/blaze-items/src/bound.rs`

- L168: `let TyKind::FnDef(def, args) = ty.kind() else { return };` → `let TyKind::FnDef(def, args) = ty.kind() else { return };`. R4: pattern unchanged; read `args` as `args.no_bound_vars().unwrap()` (once, `let args = args.no_bound_vars().unwrap();`), or pass it on unchanged where it is put back into a `FnDef`.
> forwarded to items (told by tmux; regenerated list, check each line against the code first).
> checked (items): already applied. `bound.rs:169` is `let args = args.no_bound_vars().unwrap();`.

### `crates/blaze-items/src/form.rs`

- L1351: `TyKind::FnDef(def, args) => {` → `TyKind::FnDef(def, args) => {`. R4: pattern unchanged; read `args` as `args.no_bound_vars().unwrap()` (once, `let args = args.no_bound_vars().unwrap();`), or pass it on unchanged where it is put back into a `FnDef`.
- L1447: `TyKind::Alias(is_rigid, alias) => {`. Withdrawn: this line is already ported; my regenerated suggestion was wrong. Nothing to do.
- L1976: `TyKind::Alias(_, alias) => match alias.kind {`. Withdrawn: this line is already ported; my regenerated suggestion was wrong. Nothing to do.
- L2510: `| TyKind::Alias(_, _)`. Withdrawn: this line is already ported; my regenerated suggestion was wrong. Nothing to do.
> forwarded to items (told by tmux; regenerated list, check each line against the code first).
> checked (items): already applied. `form.rs:1352` is `let args = args.no_bound_vars().unwrap();`. The withdrawn lines are as they say.

### `crates/hir-ty/src/layout.rs`

- L517: `unsized_part = TyKind::Alias(chalk_ir::AliasTy::Projection(ProjectionTy {` → `(no change: chalk, inside a comment or dead code)`
- L1085: `ty(TyKind::Alias(` → `ty(TyKind::Alias(`. R7
> forwarded to items (told by tmux; regenerated list, check each line against the code first).
> checked (items): nothing to do. L517 is inside a `/* */` comment, and L1085 already reads `TyKind::Alias(IsRigid::No, ..)`.

### `crates/hir-ty/src/specialization.rs`

- L253: `TyKind::Alias(_, alias)`. Withdrawn: this line is already ported; my regenerated suggestion was wrong. Nothing to do.
> forwarded to items (told by tmux; regenerated list, check each line against the code first).
> checked (items): already applied, as the note says.

### `crates/hir-ty/src/traits.rs`

- L302: `let clause = EarlyBinder::bind(interner, clause).instantiate(interner, args).skip_norm_wip();`. Withdrawn: this line is already ported; my regenerated suggestion was wrong. Nothing to do.
> forwarded to items (told by tmux; regenerated list, check each line against the code first).
> checked (items): already applied, as the note says.

## codegen

Window `codegen-next1`. 87 sites in 28 files.


Besides the sites below:

- Every `eval_const` caller now passes an `AliasConst` (R7).
- The MIR builders call db's new `mir_const_alias_kind`.
- `hir-ty-mir/src/mir/eval/shim/reflection.rs` (about line 662) says the solver lacks
  `TypingMode::Reflection`. It exists now (R6), so build that infcx with `TypingMode::Reflection`
  and remove the message.
> forwarded to codegen (told by tmux; regenerated list, check each line against the code first).
> forwarded to codegen (told by solver-sync-callers; told again by requests).

### `crates/hir-ty-codegen/debuginfo/types.rs`

- L175: `let infcx = interner.infer_ctxt().build(TypingMode::PostAnalysis);` → `let infcx = interner.infer_ctxt().build(TypingMode::Codegen);`. R6: codegen / monomorphization.
> forwarded to codegen (told by tmux; regenerated list, check each line against the code first).

### `crates/hir-ty-codegen/fma.rs`

- L137: `let TyKind::FnDef(def, args) = ty.kind() else { return None };` → `let TyKind::FnDef(def, args) = ty.kind() else { return None };`. R4: pattern unchanged; read `args` as `args.no_bound_vars().unwrap()` (once, `let args = args.no_bound_vars().unwrap();`), or pass it on unchanged where it is put back into a `FnDef`.
> forwarded to codegen (told by tmux; regenerated list, check each line against the code first).

### `crates/hir-ty-codegen/fncache.rs`

- L137: `let TyKind::FnDef(def, args) = ty.kind() else { return None };` → `let TyKind::FnDef(def, args) = ty.kind() else { return None };`. R4: pattern unchanged; read `args` as `args.no_bound_vars().unwrap()` (once, `let args = args.no_bound_vars().unwrap();`), or pass it on unchanged where it is put back into a `FnDef`.
> forwarded to codegen (told by tmux; regenerated list, check each line against the code first).

### `crates/hir-ty-codegen/home.rs`

- L233: `TyKind::Alias(AliasTy { kind: AliasTyKind::Opaque { def_id }, args, .. })` → `TyKind::Alias(_, AliasTy { kind: AliasTyKind::Opaque { def_id }, args, .. })`
- L254: `TyKind::Alias(AliasTy { kind: AliasTyKind::Projection { .. }, .. }) if self.reveal => {}` → `TyKind::Alias(_, AliasTy { kind: AliasTyKind::Projection { .. }, .. }) if self.reveal => {}`
- L283: `if *NO_HOME || !asked.param_env(db).caller_bounds().is_empty()` → `if *NO_HOME || !asked.param_env(db).caller_bounds().next().is_none()`. R13
> forwarded to codegen (told by tmux; regenerated list, check each line against the code first).

### `crates/hir-ty-codegen/mod.rs`

- top: `(imports)` → `use hir_ty::next_solver::{RegionExt};  // or crate::next_solver inside hir-ty`. R10: this file calls a method that moved to the trait.
- L73: `StoredTy, TraitRef, Ty, TyKind, TypingMode, UnevaluatedConst, ValTree,` → `(drop `UnevaluatedConst` from the import; add `AliasConst, AliasConstKind, IsRigid` where used)`. R7
- L3774: `if let TyKind::Alias(rustc_type_ir::AliasTy {` → `if let TyKind::Alias(_, rustc_type_ir::AliasTy {`
- L3934: `TyKind::Alias(rustc_type_ir::AliasTy {` → `TyKind::Alias(_, rustc_type_ir::AliasTy {`
- L5059: `infcx: interner.infer_ctxt().build(TypingMode::PostAnalysis),` → `infcx: interner.infer_ctxt().build(TypingMode::Codegen),`. R6: codegen / monomorphization.
- L10563: `let ConstKind::Unevaluated(unevaluated) = konst.as_ref().kind() else { return None };` → `let ConstKind::Alias(_, unevaluated) = konst.as_ref().kind() else { return None };`. R7: the binding is now an `AliasConst`, which `eval_const` takes.
- L10996: `ConstKind::Unevaluated(unevaluated) => {` → `ConstKind::Alias(_, unevaluated) => {`. R7: the binding is now an `AliasConst`, which `eval_const` takes.
- L11180: `fn eval_const(&mut self, unevaluated: UnevaluatedConst<'db>) -> Result<Allocation<'db>> {` → `fn eval_const(&mut self, unevaluated: AliasConst<DbInterner<'db>>) -> Result<Allocation<'db>> {`. R7. Body: `let AliasConst { kind, args, .. } = unevaluated;` then `match kind { Projection { def_id: TraitAssocConstId(id) } | InherentImpl { def_id: InherentAssocConstId(id) } | Free { def_id: FreeConstAliasId(id) } => { lookup_impl_const(..); db.const_eval(..) } Anon { def_id } => db.anon_const_eval(def_id.0, args, Some(self.env)), InherentSelf { .. } => Err(CodegenError::Frontend(format!("the inherent const {unevaluated:?} is in Self form after type check"))) }`. The `StaticId` arm goes. `refused_const_eval(db, def.0, &e)` takes a `GeneralConstId`: pass `GeneralConstId::ConstId(id)` / `GeneralConstId::AnonConstId(def_id.0)` from the arm (move `frontend` into the arms).
- L11182: `let UnevaluatedConst { def, args } = unevaluated;` → `let AliasConst { kind, args, .. } = unevaluated;`. R7, see the line above.
- L11862: `&& let ConstKind::Unevaluated(named) = konst.as_ref().kind() =>` → `&& let ConstKind::Alias(_, named) = konst.as_ref().kind()`. R7: the binding is now an `AliasConst`, which `eval_const` takes.
- L11971: `let ConstKind::Unevaluated(named) = konst.as_ref().kind() else {` → `let ConstKind::Alias(_, named) = konst.as_ref().kind() else {`. R7: the binding is now an `AliasConst`, which `eval_const` takes.
- L12517: `let TyKind::FnDef(def, generic_args) = fn_ty.kind() else {` → `let TyKind::FnDef(def, generic_args) = fn_ty.kind() else {`. R4: pattern unchanged; read `generic_args` as `generic_args.no_bound_vars().unwrap()` (once, `let generic_args = generic_args.no_bound_vars().unwrap();`), or pass it on unchanged where it is put back into a `FnDef`.
- L13547: `let TyKind::FnDef(def, generic_args) = fn_ty.kind() else {` → `let TyKind::FnDef(def, generic_args) = fn_ty.kind() else {`. R4: pattern unchanged; read `generic_args` as `generic_args.no_bound_vars().unwrap()` (once, `let generic_args = generic_args.no_bound_vars().unwrap();`), or pass it on unchanged where it is put back into a `FnDef`.
- L13602: `let TyKind::FnDef(runtime, runtime_args) = self.operand_ty(runtime)?.kind() else {` → `let TyKind::FnDef(runtime, runtime_args) = self.operand_ty(runtime)?.kind() else {`. R4: pattern unchanged; read `runtime_args` as `runtime_args.no_bound_vars().unwrap()` (once, `let runtime_args = runtime_args.no_bound_vars().unwrap();`), or pass it on unchanged where it is put back into a `FnDef`.
- L13812: `TyKind::FnDef(def, generic_args) => {` → `TyKind::FnDef(def, generic_args) => {`. R4: pattern unchanged; read `generic_args` as `generic_args.no_bound_vars().unwrap()` (once, `let generic_args = generic_args.no_bound_vars().unwrap();`), or pass it on unchanged where it is put back into a `FnDef`.
- L14338: `return self.reified(Ty::new_fn_def(interner, def, callee_args), false);` → `return self.reified(Ty::new_fn_def(interner, def, callee_args), Binder::dummy(false));`. R4
- L14458: `TyKind::FnDef(def, generic_args) => match def.0 {` → `TyKind::FnDef(def, generic_args) => match def.0 {`. R4: pattern unchanged; read `generic_args` as `generic_args.no_bound_vars().unwrap()` (once, `let generic_args = generic_args.no_bound_vars().unwrap();`), or pass it on unchanged where it is put back into a `FnDef`.
- L14515: `Ty::new_fn_def(self.interner, CallableDefId::FunctionId(func).into(), args)` → `Ty::new_fn_def(self.interner, CallableDefId::FunctionId(func).into(), Binder::dummy(args))`. R4
- L17211: `ConstKind::Unevaluated(named) => self.eval_const(named)?.memory.to_vec(),` → `ConstKind::Alias(_, named) => self.eval_const(named)?.memory.to_vec(),`. R7: the binding is now an `AliasConst`, which `eval_const` takes.
> forwarded to codegen (told by tmux; regenerated list, check each line against the code first).

### `crates/hir-ty-codegen/stable.rs`

- L745: `TyKind::FnDef(def, args) => {` → `TyKind::FnDef(def, args) => {`. R4: pattern unchanged; read `args` as `args.no_bound_vars().unwrap()` (once, `let args = args.no_bound_vars().unwrap();`), or pass it on unchanged where it is put back into a `FnDef`.
- L829: `TyKind::Alias(AliasTy { kind: AliasTyKind::Projection { def_id }, args, .. }) => {` → `TyKind::Alias(_, AliasTy { kind: AliasTyKind::Projection { def_id }, args, .. }) => {`
- L836: `TyKind::Alias(AliasTy { kind: AliasTyKind::Opaque { def_id }, args, .. }) => {` → `TyKind::Alias(_, AliasTy { kind: AliasTyKind::Opaque { def_id }, args, .. }) => {`
> forwarded to codegen (told by tmux; regenerated list, check each line against the code first).

### `crates/hir-ty-codegen/sve.rs`

- L1314: `ConstKind::Unevaluated(named) => {` → `ConstKind::Alias(_, named) => {`. R7: the binding is now an `AliasConst`, which `eval_const` takes.
> forwarded to codegen (told by tmux; regenerated list, check each line against the code first).

### `crates/hir-ty-codegen/type_id.rs`

- L575: `TyKind::FnDef(def, args) => {` → `TyKind::FnDef(def, args) => {`. R4: pattern unchanged; read `args` as `args.no_bound_vars().unwrap()` (once, `let args = args.no_bound_vars().unwrap();`), or pass it on unchanged where it is put back into a `FnDef`.
> forwarded to codegen (told by tmux; regenerated list, check each line against the code first).

### `crates/hir-ty-codegen/v2/hashes.rs`

- L474: `TyKind::FnDef(def, args) => {` → `TyKind::FnDef(def, args) => {`. R4: pattern unchanged; read `args` as `args.no_bound_vars().unwrap()` (once, `let args = args.no_bound_vars().unwrap();`), or pass it on unchanged where it is put back into a `FnDef`.
- L602: `TyKind::Alias(alias) => {` → `TyKind::Alias(_, alias) => {`
- L686: `ConstKind::Unevaluated(_) => {` → `ConstKind::Alias(..) => {`. R7: the binding is now an `AliasConst`, which `eval_const` takes.
> forwarded to codegen (told by tmux; regenerated list, check each line against the code first).

### `crates/hir-ty-codegen/v2/places.rs`

- L314: `let infcx = DbInterner::new_with(db, krate).infer_ctxt().build(TypingMode::PostAnalysis);` → `let infcx = DbInterner::new_with(db, krate).infer_ctxt().build(TypingMode::Codegen);`. R6: codegen / monomorphization.
> forwarded to codegen (told by tmux; regenerated list, check each line against the code first).

### `crates/hir-ty-codegen/v2/refs.rs`

- top: `(imports)` → `use hir_ty::next_solver::{RegionExt};  // or crate::next_solver inside hir-ty`. R10: this file calls a method that moved to the trait.
- L529: `ConstKind::Unevaluated(unevaluated) => {` → `ConstKind::Alias(_, unevaluated) => {`. R7: the binding is now an `AliasConst`, which `eval_const` takes.
- L1114: `let TyKind::FnDef(def, generic_args) = fn_ty.kind() else {` → `let TyKind::FnDef(def, generic_args) = fn_ty.kind() else {`. R4: pattern unchanged; read `generic_args` as `generic_args.no_bound_vars().unwrap()` (once, `let generic_args = generic_args.no_bound_vars().unwrap();`), or pass it on unchanged where it is put back into a `FnDef`.
- L1150: `let TyKind::FnDef(runtime, runtime_args) = self.operand_ty(runtime).kind() else {` → `let TyKind::FnDef(runtime, runtime_args) = self.operand_ty(runtime).kind() else {`. R4: pattern unchanged; read `runtime_args` as `runtime_args.no_bound_vars().unwrap()` (once, `let runtime_args = runtime_args.no_bound_vars().unwrap();`), or pass it on unchanged where it is put back into a `FnDef`.
- L1239: `TyKind::FnDef(def, generic_args) => match def.0 {` → `TyKind::FnDef(def, generic_args) => match def.0 {`. R4: pattern unchanged; read `generic_args` as `generic_args.no_bound_vars().unwrap()` (once, `let generic_args = generic_args.no_bound_vars().unwrap();`), or pass it on unchanged where it is put back into a `FnDef`.
- L1367: `&& let ConstKind::Unevaluated(named) = konst.as_ref().kind() =>` → `&& let ConstKind::Alias(_, named) = konst.as_ref().kind() =>`. R7
- L1418: `Ty::new_fn_def(interner, CallableDefId::FunctionId(func).into(), args)` → `Ty::new_fn_def(interner, CallableDefId::FunctionId(func).into(), Binder::dummy(args))`. R4
> forwarded to codegen (told by tmux; regenerated list, check each line against the code first).

### `crates/hir-ty-codegen/v2/refs/intrinsics.rs`

- L1390: `let ConstKind::Unevaluated(named) = konst.as_ref().kind() else {` → `let ConstKind::Alias(_, named) = konst.as_ref().kind() else {`. R7: the binding is now an `AliasConst`, which `eval_const` takes.
- L1428: `ConstKind::Unevaluated(named) => {` → `ConstKind::Alias(_, named) => {`. R7: the binding is now an `AliasConst`, which `eval_const` takes.
- L1633: `ConstKind::Unevaluated(named) => {` → `ConstKind::Alias(_, named) => {`. R7: the binding is now an `AliasConst`, which `eval_const` takes.
> forwarded to codegen (told by tmux; regenerated list, check each line against the code first).

### `crates/hir-ty-codegen/v2/refs/shims.rs`

- top: `(imports)` → `use hir_ty::next_solver::{RegionExt};  // or crate::next_solver inside hir-ty`. R10: this file calls a method that moved to the trait.
- L330: `TyKind::FnDef(def, generic_args) => match def.0 {` → `TyKind::FnDef(def, generic_args) => match def.0 {`. R4: pattern unchanged; read `generic_args` as `generic_args.no_bound_vars().unwrap()` (once, `let generic_args = generic_args.no_bound_vars().unwrap();`), or pass it on unchanged where it is put back into a `FnDef`.
> forwarded to codegen (told by tmux; regenerated list, check each line against the code first).

### `crates/hir-ty-codegen/vtable.rs`

- top: `(imports)` → `use hir_ty::next_solver::{RegionExt};  // or crate::next_solver inside hir-ty`. R10: this file calls a method that moved to the trait.
- L100: `.explicit_super_predicates_of(trait_ref.def_id)` → `.explicit_super_clauses_of(trait_ref.def_id)`
> forwarded to codegen (told by tmux; regenerated list, check each line against the code first).

### `crates/hir-ty-codegen/wasm_simd.rs`

- L390: `ConstKind::Unevaluated(named) => Some(self.eval_const(named)?.memory.to_vec()),` → `ConstKind::Alias(_, named) =>`. R7: the binding is now an `AliasConst`, which `eval_const` takes.
> forwarded to codegen (told by tmux; regenerated list, check each line against the code first).

### `crates/hir-ty-mir/src/mir/eval.rs`

- L52: `GenericArgs, Region, Ty, TyKind, TypingMode, UnevaluatedConst, ValTree,` → `(drop `UnevaluatedConst` from the import; add `AliasConst, AliasConstKind, IsRigid` where used)`. R7
- L1123: `(TyKind::FnDef(def, generic_args), Some(intrinsic)) => {` → `(TyKind::FnDef(def, generic_args), Some(intrinsic)) => {`. R4: pattern unchanged; read `generic_args` as `generic_args.no_bound_vars().unwrap()` (once, `let generic_args = generic_args.no_bound_vars().unwrap();`), or pass it on unchanged where it is put back into a `FnDef`.
- L1140: `(TyKind::FnDef(def, generic_args), None) => self.exec_fn_def(` → `(TyKind::FnDef(def, generic_args), None) => self.exec_fn_def(`. R4: pattern unchanged; read `generic_args` as `generic_args.no_bound_vars().unwrap()` (once, `let generic_args = generic_args.no_bound_vars().unwrap();`), or pass it on unchanged where it is put back into a `FnDef`.
- L2508: `ConstKind::Unevaluated(UnevaluatedConst { def: const_id, args: subst }) => {` → `ConstKind::Alias(_, AliasConst { kind, args: subst, .. }) => {`. R7. Body: `match kind { Projection { def_id: TraitAssocConstId(c) } | InherentImpl { def_id: InherentAssocConstId(c) } | Free { def_id: FreeConstAliasId(c) } => { let (c, s) = lookup_impl_const(..); self.db.const_eval(c, s, ..) } Anon { def_id } if def_id.0.is_inline_const(self.db) => return self.inline_const_in_heap(def_id.0, subst, locals), Anon { def_id } => self.db.anon_const_eval(def_id.0, ..), InherentSelf { .. } => return Err(MirEvalError::..("an inherent const in Self form after type check")) }`. The `StaticId` arm goes. `id.name(self.db)` for the error needs a `GeneralConstId` built from the arm.
- L2554: `ConstKind::Unevaluated(UnevaluatedConst { def, .. }) => match def.0 {` → `ConstKind::Alias(..) => true,`. R7: replaces the whole inner match (`ConstId | AnonConstId => true, StaticId => false`): an alias const is never a static.
- L3355: `TyKind::Alias(AliasTy { kind: AliasTyKind::Projection { .. }, .. }) => {` → `TyKind::Alias(_, AliasTy { kind: AliasTyKind::Projection { .. }, .. }) => {`
- L3678: `TyKind::FnDef(def, generic_args) => {` → `TyKind::FnDef(def, generic_args) => {`. R4: pattern unchanged; read `generic_args` as `generic_args.no_bound_vars().unwrap()` (once, `let generic_args = generic_args.no_bound_vars().unwrap();`), or pass it on unchanged where it is put back into a `FnDef`.
- L4024: `TyKind::FnDef(def, subst) => {` → `TyKind::FnDef(def, subst) => {`. R4: pattern unchanged; read `subst` as `subst.no_bound_vars().unwrap()` (once, `let subst = subst.no_bound_vars().unwrap();`), or pass it on unchanged where it is put back into a `FnDef`.
- L4440: `let debug_fmt_fn_ptr = evaluator.vtable_map.id(Ty::new_fn_def(` → `let debug_fmt_fn_ptr = evaluator.vtable_map.id(Ty::new_fn_def(`. R4: the call spans lines; wrap its last argument (the args) in `Binder::dummy(..)`.
> forwarded to codegen (told by tmux; regenerated list, check each line against the code first).

### `crates/hir-ty-mir/src/mir/eval/shim.rs`

- top: `(imports)` → `use hir_ty::next_solver::{RegionExt};  // or crate::next_solver inside hir-ty`. R10: this file calls a method that moved to the trait.
> forwarded to codegen (told by tmux; regenerated list, check each line against the code first).

### `crates/hir-ty-mir/src/mir/lower.rs`

- top: `(imports)` → `use hir_ty::next_solver::{RegionExt};  // or crate::next_solver inside hir-ty`. R10: this file calls a method that moved to the trait.
- L63: `UnevaluatedConst,` → `(drop `UnevaluatedConst` from the import; add `AliasConst, AliasConstKind, IsRigid` where used)`. R7
- L1055: `let ty = Ty::new_fn_def(` → `let ty = Ty::new_fn_def(`. R4: the call spans lines; wrap its last argument (the args) in `Binder::dummy(..)`.
- L2270: `let konst = Const::new_unevaluated(`

  ```rust
  let kind = match const_id {
      GeneralConstId::ConstId(id) => mir_const_alias_kind(self.db, id),
      GeneralConstId::AnonConstId(id) => AliasConstKind::Anon { def_id: AnonConstIdWrapper(id) },
      GeneralConstId::StaticId(id) => panic!("the static {id:?} is not a const"),
  };
  let konst = Const::new_alias(self.interner(), IsRigid::No, AliasConst::new(self.interner(), kind, subst));
  ```

  R7, R3. Check the callers of this fn: a static must reach MIR as a place (`Operand::Static`), never here; if one does today, that is a bug the panic now names. The `UnevaluatedConst { def: const_id.into(), args: subst }` line below folds in.

- L2272: `UnevaluatedConst { def: const_id.into(), args: subst },` → `(removed, see above)`. R7
- L3877: `let future_ty = Ty::new_projection(interner, future_alias.into(), [awaitee_ty]);` → `let future_ty = Ty::new_projection(interner, IsRigid::No, future_alias.into(), [awaitee_ty]);`. R3
- L3895: `Operand::const_zst(Ty::new_fn_def(` → `Operand::const_zst(Ty::new_fn_def(`. R4: the call spans lines; wrap its last argument (the args) in `Binder::dummy(..)`.
> forwarded to codegen (told by tmux; regenerated list, check each line against the code first).

### `crates/hir-ty-mir/src/mir/lower/as_place.rs`

- top: `(imports)` → `use hir_ty::next_solver::{RegionExt};  // or crate::next_solver inside hir-ty`. R10: this file calls a method that moved to the trait.
- L300: `let index_fn_op = Operand::const_zst(Ty::new_fn_def(` → `let index_fn_op = Operand::const_zst(Ty::new_fn_def(`. R4: the call spans lines; wrap its last argument (the args) in `Binder::dummy(..)`.
- L348: `let deref_fn_op = Operand::const_zst(Ty::new_fn_def(` → `let deref_fn_op = Operand::const_zst(Ty::new_fn_def(`. R4: the call spans lines; wrap its last argument (the args) in `Binder::dummy(..)`.
> forwarded to codegen (told by tmux; regenerated list, check each line against the code first).

### `crates/hir-ty-mir/src/mir/lower/deref_pattern.rs`

- top: `(imports)` → `use hir_ty::next_solver::{RegionExt};  // or crate::next_solver inside hir-ty`. R10: this file calls a method that moved to the trait.
- L71: `let func = Operand::const_zst(Ty::new_fn_def(` → `let func = Operand::const_zst(Ty::new_fn_def(`. R4: the call spans lines; wrap its last argument (the args) in `Binder::dummy(..)`.
- L90: `let target = Ty::new_projection(self.interner(), target.into(), [pointer]);` → `let target = Ty::new_projection(self.interner(), IsRigid::No, target.into(), [pointer]);`. R3
> forwarded to codegen (told by tmux; regenerated list, check each line against the code first).

### `crates/hir-ty-mir/src/mir/lower/pattern_matching.rs`

- top: `(imports)` → `use hir_ty::next_solver::{RegionExt};  // or crate::next_solver inside hir-ty`. R10: this file calls a method that moved to the trait.
> forwarded to codegen (told by tmux; regenerated list, check each line against the code first).

### `crates/hir-ty-mir/src/mir/monomorphization.rs`

- top: `(imports)` → `use hir_ty::next_solver::{RegionExt};  // or crate::next_solver inside hir-ty`. R10: this file calls a method that moved to the trait.
- L150: `let infcx = interner.infer_ctxt().build(TypingMode::PostAnalysis);` → `let infcx = interner.infer_ctxt().build(TypingMode::Codegen);`. R6: codegen / monomorphization.
- L157: `let TyKind::Alias(alias) = ty.kind() else { return ty };` → `let TyKind::Alias(_, alias) = ty.kind() else { return ty };`
- L169: `let TyKind::Alias(alias) = ty.kind() else { return None };` → `let TyKind::Alias(_, alias) = ty.kind() else { return None };`
- L434: `let TyKind::Alias(alias) = ty.kind() else { return None };` → `let TyKind::Alias(_, alias) = ty.kind() else { return None };`
> forwarded to codegen (told by tmux; regenerated list, check each line against the code first).

### `crates/hir-ty-mir/src/mir/optimize/check_pointers.rs`

- L33: `next_solver::{Const, GenericArgs, Ty, TyKind, UnevaluatedConst},` → `},  // drop UnevaluatedConst; add AliasConst, AliasConstKind, IsRigid`. R7
- L456: `let konst = Const::new_unevaluated(`

  ```rust
  let konst = Const::new_alias(
      ctx.infcx.interner,
      IsRigid::No,
      AliasConst::new(ctx.infcx.interner, mir_const_alias_kind(ctx.db, item), args),
  );
  ```

  R7, R3: `item` is a `ConstId`; the next line folds in.

- L458: `UnevaluatedConst { def: GeneralConstId::ConstId(item).into(), args },` → `(removed, see above)`. R7
> forwarded to codegen (told by tmux; regenerated list, check each line against the code first).

### `crates/hir-ty-mir/src/mir/optimize/inline.rs`

- top: `(imports)` → `use hir_ty::next_solver::{RegionExt};  // or crate::next_solver inside hir-ty`. R10: this file calls a method that moved to the trait.
- L722: `TyKind::FnDef(def, args) => match def.0 {` → `TyKind::FnDef(def, args) => match def.0 {`. R4: pattern unchanged; read `args` as `args.no_bound_vars().unwrap()` (once, `let args = args.no_bound_vars().unwrap();`), or pass it on unchanged where it is put back into a `FnDef`.
> forwarded to codegen (told by tmux; regenerated list, check each line against the code first).

### `crates/hir-ty-mir/src/mir/optimize/inline/integrate.rs`

- top: `(imports)` → `use hir_ty::next_solver::{RegionExt};  // or crate::next_solver inside hir-ty`. R10: this file calls a method that moved to the trait.
> forwarded to codegen (told by tmux; regenerated list, check each line against the code first).

### `crates/hir-ty-mir/src/mir/purity.rs`

- L217: `let TyKind::FnDef(def, args) = ty.kind() else {` → `let TyKind::FnDef(def, args) = ty.kind() else {`. R4: pattern unchanged; read `args` as `args.no_bound_vars().unwrap()` (once, `let args = args.no_bound_vars().unwrap();`), or pass it on unchanged where it is put back into a `FnDef`.
> forwarded to codegen (told by tmux; regenerated list, check each line against the code first).

### `crates/hir-ty-mir/src/mir/transform/elaborate_drops.rs`

- top: `(imports)` → `use hir_ty::next_solver::{RegionExt};  // or crate::next_solver inside hir-ty`. R10: this file calls a method that moved to the trait.
> forwarded to codegen (told by tmux; regenerated list, check each line against the code first).

### `crates/hir-ty-mir/src/mir/type_name.rs`

- L436: `TyKind::FnDef(def, args) => match def.0 {` → `TyKind::FnDef(def, args) => match def.0 {`. R4: pattern unchanged; read `args` as `args.no_bound_vars().unwrap()` (once, `let args = args.no_bound_vars().unwrap();`), or pass it on unchanged where it is put back into a `FnDef`.
> forwarded to codegen (told by tmux; regenerated list, check each line against the code first).

## diagnostics

Window `diagnostics`. 60 sites in 17 files.
> forwarded to diagnostics (told by tmux; regenerated list, check each line against the code first).
> forwarded to diagnostics (told by solver-sync-callers; told again by requests).

### `crates/blaze-diagnostics/src/lints/bodies/calls/methods/noop_method_call.rs`

- top: `(imports)` → `use hir_ty::next_solver::{RegionExt};  // or crate::next_solver inside hir-ty`. R10: this file calls a method that moved to the trait.
> forwarded to diagnostics (told by tmux; regenerated list, check each line against the code first).

### `crates/blaze-diagnostics/src/lints/bodies/calls/statements.rs`

- L389: `let TyKind::Alias(AliasTy { kind: AliasTyKind::Opaque { def_id }, .. }) = b.infer.expr_ty(future).kind() else {` → `let TyKind::Alias(_, AliasTy { kind: AliasTyKind::Opaque { def_id }, .. }) = b.infer.expr_ty(future).kind() else {`
- L445: `TyKind::Alias(alias) => match alias.kind {` → `TyKind::Alias(_, alias) => match alias.kind {`
- L448: `let bounds = interner.item_self_bounds(alias.kind.def_id()).skip_binder();`

  ```rust
  let def_id = match alias.kind {
      AliasTyKind::Opaque { def_id } => def_id.into(),
      AliasTyKind::Projection { def_id } => def_id.into(),
      _ => unreachable!(),
  };
  let bounds = interner.item_self_bounds(def_id).skip_binder();
  ```

  R8; or split the `Opaque { .. } | Projection { .. }` arm above into two arms that bind `def_id`.
> forwarded to diagnostics (told by tmux; regenerated list, check each line against the code first).

### `crates/blaze-diagnostics/src/lints/bodies/liveness.rs`

- L342: `| TyKind::Alias(AliasTy { kind: AliasTyKind::Opaque { .. }, .. })` → `| TyKind::Alias(_, AliasTy { kind: AliasTyKind::Opaque { .. }, .. })`
> forwarded to diagnostics (told by tmux; regenerated list, check each line against the code first).

### `crates/blaze-diagnostics/src/lints/bodies/panics.rs`

- L160: `let TyKind::FnDef(CallableIdWrapper(callable), args) = ty.kind() else { return None };` → `let TyKind::FnDef(CallableIdWrapper(callable), args) = ty.kind() else { return None };`. R4: pattern unchanged; read `args` as `args.no_bound_vars().unwrap()` (once, `let args = args.no_bound_vars().unwrap();`), or pass it on unchanged where it is put back into a `FnDef`.
- L807: `ConstKind::Unevaluated(unevaluated) => match unevaluated.def.0 {` → `ConstKind::Alias(_, unevaluated) => match unevaluated.kind {`. R7. `Projection { def_id: TraitAssocConstId(id) } | InherentImpl { def_id: InherentAssocConstId(id) } | Free { def_id: FreeConstAliasId(id) } => { lookup_impl_const(.., id, unevaluated.args) ... }`, `Anon { def_id } => self.db.anon_const_eval(def_id.0, unevaluated.args, None).ok()?`, `InherentSelf { .. } => return None` (a lint reads, it does not stop the build; R7 says post-typeck MIR holds no Self-form const). Delete the `StaticId` arm.
> forwarded to diagnostics (told by tmux; regenerated list, check each line against the code first).

### `crates/blaze-diagnostics/src/lints/bodies/recursion.rs`

- L217: `let TyKind::FnDef(CallableIdWrapper(CallableDefId::FunctionId(callee)), call_args) = ty.kind() else {` → `let TyKind::FnDef(CallableIdWrapper(CallableDefId::FunctionId(callee)), call_args) = ty.kind() else {`. R4: pattern unchanged; read `call_args` as `call_args.no_bound_vars().unwrap()` (once, `let call_args = call_args.no_bound_vars().unwrap();`), or pass it on unchanged where it is put back into a `FnDef`.
> forwarded to diagnostics (told by tmux; regenerated list, check each line against the code first).

### `crates/blaze-diagnostics/src/lints/bodies/types/values.rs`

- top: `(imports)` → `use hir_ty::next_solver::{RegionExt};  // or crate::next_solver inside hir-ty`. R10: this file calls a method that moved to the trait.
> forwarded to diagnostics (told by tmux; regenerated list, check each line against the code first).

### `crates/blaze-diagnostics/src/lints/bodies/types/values/inhabited.rs`

- L75: `TyKind::Alias(AliasTy { kind, .. }) => match kind {` → `TyKind::Alias(_, AliasTy { kind, .. }) => match kind {`
- L264: `let c = EarlyBinder::bind(c).instantiate(interner, args).skip_norm_wip();` → `let c = EarlyBinder::bind(interner, c).instantiate(interner, args).skip_norm_wip();`. R5
- L272: `let t = EarlyBinder::bind(t).instantiate(interner, args).skip_norm_wip();` → `let t = EarlyBinder::bind(interner, t).instantiate(interner, args).skip_norm_wip();`. R5
> forwarded to diagnostics (told by tmux; regenerated list, check each line against the code first).

### `crates/blaze-diagnostics/src/lints/bodies/types/values/intrinsics.rs`

- L49: `let TyKind::FnDef(_, fn_args) = util::ty(b, callee).kind() else { return };` → `let TyKind::FnDef(_, fn_args) = util::ty(b, callee).kind() else { return };`. R4: pattern unchanged; read `fn_args` as `fn_args.no_bound_vars().unwrap()` (once, `let fn_args = fn_args.no_bound_vars().unwrap();`), or pass it on unchanged where it is put back into a `FnDef`.
> forwarded to diagnostics (told by tmux; regenerated list, check each line against the code first).

### `crates/blaze-diagnostics/src/lints/bodies/types/values/invalid.rs`

- L265: `let field_ty = EarlyBinder::bind(types[field].ty().skip_binder()).instantiate(interner, args).skip_norm_wip();` → `let field_ty = EarlyBinder::bind(interner, types[field].ty().skip_binder()).instantiate(interner, args).skip_norm_wip();`. R5
> forwarded to diagnostics (told by tmux; regenerated list, check each line against the code first).

### `crates/blaze-diagnostics/src/lints/bodies/types/values/transmutes.rs`

- L64: `let TyKind::FnDef(_, args) = util::ty(b, path).kind() else { return None };` → `let TyKind::FnDef(_, args) = util::ty(b, path).kind() else { return None };`. R4: pattern unchanged; read `args` as `args.no_bound_vars().unwrap()` (once, `let args = args.no_bound_vars().unwrap();`), or pass it on unchanged where it is put back into a `FnDef`.
> forwarded to diagnostics (told by tmux; regenerated list, check each line against the code first).

### `crates/blaze-diagnostics/src/lints/names/items/externs.rs`

- top: `(imports)` → `use hir_ty::next_solver::{RegionExt};  // or crate::next_solver inside hir-ty`. R10: this file calls a method that moved to the trait.
- L610: `TyKind::Alias(AliasTy { kind: AliasTyKind::Opaque { .. }, .. }) => {` → `TyKind::Alias(_, AliasTy { kind: AliasTyKind::Opaque { .. }, .. }) => {`
- L613: `TyKind::Param(_) | TyKind::Alias(AliasTy { kind: AliasTyKind::Projection { .. } | AliasTyKind::Inherent { .. }, .. })` → `TyKind::Param(_) | TyKind::Alias(_, AliasTy { kind: AliasTyKind::Projection { .. } | AliasTyKind::Inherent { .. }, .. })`
- L626: `| TyKind::Alias(_)` → `| TyKind::Alias(_, _)`
- L640: `.find(|it| matches!(it.kind(), TyKind::Alias(AliasTy { kind: AliasTyKind::Opaque { .. }, .. })))` → `.find(|it| matches!(it.kind(), TyKind::Alias(_, AliasTy { kind: AliasTyKind::Opaque { .. }, .. })))`
- L1089: `(TyKind::FnDef(a_def, a_args), TyKind::FnDef(b_def, b_args)) => {` → `(TyKind::FnDef(a_def, a_args), TyKind::FnDef(b_def, b_args)) => {`. R4: pattern unchanged; read `a_args` as `a_args.no_bound_vars().unwrap()` (once, `let a_args = a_args.no_bound_vars().unwrap();`), or pass it on unchanged where it is put back into a `FnDef`.
> forwarded to diagnostics (told by tmux; regenerated list, check each line against the code first).

### `crates/blaze-diagnostics/src/lints/names/items/runtime_symbols.rs`

- top: `(imports)` → `use hir_ty::next_solver::{RegionExt};  // or crate::next_solver inside hir-ty`. R10: this file calls a method that moved to the trait.
> forwarded to diagnostics (told by tmux; regenerated list, check each line against the code first).

### `crates/blaze-diagnostics/src/lower/unimplemented_trait.rs`

- L121: `&& leaf.polarity() == hir::PredicatePolarity::Positive` → `&& leaf.polarity() == hir::ClausePolarity::Positive`
- L149: `fn explanation(cx: &Cx<'_, '_>, pred: &hir::TraitPredicate<'_>) -> String {` → `fn explanation(cx: &Cx<'_, '_>, pred: &hir::TraitClause<'_>) -> String {`
- L151: `if pred.polarity() == hir::PredicatePolarity::Negative {` → `if pred.polarity() == hir::ClausePolarity::Negative {`
- L251: `let required = |pred: &hir::TraitPredicate<'_>| {` → `let required = |pred: &hir::TraitClause<'_>| {`
- L432: `fn on_unimplemented(cx: &Cx<'_, '_>, pred: &hir::TraitPredicate<'_>, direct: bool) -> Custom {` → `fn on_unimplemented(cx: &Cx<'_, '_>, pred: &hir::TraitClause<'_>, direct: bool) -> Custom {`
- L557: `fn of(cx: &Cx<'_, '_>, pred: &hir::TraitPredicate<'_>) -> Args {` → `fn of(cx: &Cx<'_, '_>, pred: &hir::TraitClause<'_>) -> Args {`
- L729: `fn can_suggest_derive(cx: &Cx<'_, '_>, pred: &hir::TraitPredicate<'_>) -> bool {` → `fn can_suggest_derive(cx: &Cx<'_, '_>, pred: &hir::TraitClause<'_>) -> bool {`
- L730: `if pred.polarity() == hir::PredicatePolarity::Negative {` → `if pred.polarity() == hir::ClausePolarity::Negative {`
- L769: `fn suggest_derive(cx: &Cx<'_, '_>, pred: &hir::TraitPredicate<'_>, diag: &mut Diag) {` → `fn suggest_derive(cx: &Cx<'_, '_>, pred: &hir::TraitClause<'_>, diag: &mut Diag) {`
> forwarded to diagnostics (told by tmux; regenerated list, check each line against the code first).

### `crates/blaze-diagnostics/src/print.rs`

- L112: `pub fn trait_path_sugared(cx: &Cx<'_, '_>, pred: &hir::TraitPredicate<'_>) -> String {` → `pub fn trait_path_sugared(cx: &Cx<'_, '_>, pred: &hir::TraitClause<'_>) -> String {`
- L115: `if pred.polarity == rustc_type_ir::PredicatePolarity::Negative {` → `if pred.polarity == rustc_type_ir::ClausePolarity::Negative {`
- L123: `pub fn predicate(cx: &Cx<'_, '_>, pred: &hir::TraitPredicate<'_>) -> String {` → `pub fn predicate(cx: &Cx<'_, '_>, pred: &hir::TraitClause<'_>) -> String {`
> forwarded to diagnostics (told by tmux; regenerated list, check each line against the code first).

### `crates/blaze-diagnostics/src/print/consts.rs`

- L30: `ConstKind::Unevaluated(uv) => match uv.def.0 {` → `ConstKind::Alias(_, uv) => match uv.kind {`. R7. Arms: `AliasConstKind::Projection { def_id: TraitAssocConstId(it) } | AliasConstKind::InherentImpl { def_id: InherentAssocConstId(it) } | AliasConstKind::InherentSelf { def_id: InherentAssocConstId(it) } | AliasConstKind::Free { def_id: FreeConstAliasId(it) } => self.print_value_path(Def::Const(it), uv.args.as_slice())`; `AliasConstKind::Anon { def_id } => { let loc = def_id.0.loc(self.db); ... }`. Delete the `GeneralConstId::StaticId` arm: a static is never an alias const.
- L107: `(_, TyKind::FnDef(def, args)) => {` → `(_, TyKind::FnDef(def, args)) => {`. R4: pattern unchanged; read `args` as `args.no_bound_vars().unwrap()` (once, `let args = args.no_bound_vars().unwrap();`), or pass it on unchanged where it is put back into a `FnDef`.
> forwarded to diagnostics (told by tmux; regenerated list, check each line against the code first).

### `crates/blaze-diagnostics/src/print/printer.rs`

- L14: `Region, RegionKind, TraitPredicate, TraitRef, Ty, TyKind,` → `Region, RegionKind, TraitClause, TraitRef, Ty, TyKind,`
- L19: `BoundRegionKind, BoundVarIndexKind, DebruijnIndex, INNERMOST, PredicatePolarity,` → `BoundRegionKind, BoundVarIndexKind, DebruijnIndex, INNERMOST, ClausePolarity,`
- L677: `pub(crate) fn print_trait_predicate(&mut self, pred: TraitPredicate<'t>) {` → `pub(crate) fn print_trait_predicate(&mut self, pred: TraitClause<'t>) {`
- L680: `if pred.polarity == PredicatePolarity::Negative {` → `if pred.polarity == ClausePolarity::Negative {`
> forwarded to diagnostics (told by tmux; regenerated list, check each line against the code first).

### `crates/blaze-diagnostics/src/print/types.rs`

- top: `(imports)` → `use hir_ty::next_solver::{RegionExt};  // or crate::next_solver inside hir-ty`. R10: this file calls a method that moved to the trait.
- L19: `Term, TraitIdWrapper, TraitPredicate, TraitRef, Ty, TyKind, Tys,` → `Term, TraitIdWrapper, TraitClause, TraitRef, Ty, TyKind, Tys,`
- L24: `AliasTyKind, BoundTyKind, ClosureKind, InferTy, Mutability, PredicatePolarity, Upcast as _,` → `AliasTyKind, BoundTyKind, ClosureKind, InferTy, Mutability, ClausePolarity, Upcast as _,`
- L57: `type Traits<'t> = Vec<(Binder<'t, TraitPredicate<'t>>, SmallVec<[(TermId, Term<'t>); 2]>)>;` → `type Traits<'t> = Vec<(Binder<'t, TraitClause<'t>>, SmallVec<[(TermId, Term<'t>); 2]>)>;`
- L91: `| TyKind::Alias(_)` → `| TyKind::Alias(_, _)`
- L161: `TyKind::FnDef(def, args) => self.print_fn_def(def, args),` → `TyKind::FnDef(def, args) => self.print_fn_def(def, args),`. R4: pattern unchanged; read `args` as `args.no_bound_vars().unwrap()` (once, `let args = args.no_bound_vars().unwrap();`), or pass it on unchanged where it is put back into a `FnDef`.
- L196: `TyKind::Alias(alias) => match alias.kind {` → `TyKind::Alias(_, alias) => match alias.kind {`
- L388: `TyKind::Alias(it) if matches!(it.kind, AliasTyKind::Opaque { def_id } if def_id.0 == id)` → `TyKind::Alias(_, it) if matches!(it.kind, AliasTyKind::Opaque { def_id } if def_id.0 == id)`
- L422: `PredicatePolarity::Positive => {` → `ClausePolarity::Positive => {`
- L426: `PredicatePolarity::Negative => has_negative_sized = true,` → `ClausePolarity::Negative => has_negative_sized = true,`
- L435: `let trait_pred = TraitPredicate {` → `let trait_pred = TraitClause {`
- L437: `polarity: PredicatePolarity::Positive,` → `polarity: ClausePolarity::Positive,`
- L480: `let pred = entry.bound.map_bound(|(args, self_ty)| TraitPredicate {` → `let pred = entry.bound.map_bound(|(args, self_ty)| TraitClause {`
- L486: `polarity: PredicatePolarity::Positive,` → `polarity: ClausePolarity::Positive,`
- L501: `if pred.polarity == PredicatePolarity::Negative {` → `if pred.polarity == ClausePolarity::Negative {`
- L557: `pred: Binder<'t, TraitPredicate<'t>>,` → `pred: Binder<'t, TraitClause<'t>>,`
- L564: `if pred.skip_binder().polarity == PredicatePolarity::Positive` → `if pred.skip_binder().polarity == ClausePolarity::Positive`
- L885: `TyKind::Alias(alias) if matches!(alias.kind, AliasTyKind::Projection { .. } | AliasTyKind::Inherent { .. }) => {` → `TyKind::Alias(_, alias) if matches!(alias.kind, AliasTyKind::Projection { .. } | AliasTyKind::Inherent { .. }) => {`
- L889: `TyKind::Alias(alias) if let AliasTyKind::Opaque { def_id } = alias.kind => {` → `TyKind::Alias(_, alias) if let AliasTyKind::Opaque { def_id } = alias.kind => {`
- L941: `TyKind::Alias(alias) => match alias.kind {` → `TyKind::Alias(_, alias) => match alias.kind {`
> forwarded to diagnostics (told by tmux; regenerated list, check each line against the code first).

## safety

Window `safety`. 0 sites in 0 files.


Nothing to change. hir-ty-ub's two sites need no edit: `check/valid.rs` builds a `PostAnalysis`
infcx for validity checks, and R6 keeps that mode. Its `TyKind::FnDef(..)` pattern is unchanged
(R4). I am not messaging this window.
> forwarded to safety (told by tmux; regenerated list, check each line against the code first).
> applied: nothing to change.

## fastcompile-ident-share

Window `fastcompile-ident-share-next2`. 2 sites in 1 files.
> forwarded to fastcompile-ident-share (told by tmux; regenerated list, check each line against the code first).
> forwarded to fastcompile-ident-share (window 190).

### `crates/hir-ty-share/src/identity.rs`

- L203: `TyKind::FnDef(def, args) => {` → `TyKind::FnDef(def, args) => {`. R4: pattern unchanged; read `args` as `args.no_bound_vars().unwrap()` (once, `let args = args.no_bound_vars().unwrap();`), or pass it on unchanged where it is put back into a `FnDef`.
  > already ported (fastcompile-ident-share, 19:50): L204 is `let args = args.no_bound_vars().unwrap();`, called once, so nothing was applied.
- L328: `TyKind::Alias(_, alias) => {`. Withdrawn: this line is already ported; my regenerated suggestion was wrong. Nothing to do.
  > already ported (fastcompile-ident-share, 19:50): L328 is `TyKind::Alias(_, alias)` with two fields, so nothing was applied. L412 is `ConstKind::Alias(..)`, with no `Unevaluated` arm.
> forwarded to fastcompile-ident-share (told by tmux; regenerated list, check each line against the code first).
> applied (fastcompile-ident-share, 2026-10-29): nothing left. Each site is ported on disk: `identity.rs` `FnDef` reads `args.no_bound_vars().unwrap()` once (L204), and `TyKind::Alias(_, alias)` (L328). In `walk.rs`, the imports name `AliasConst`, `AliasConstKind`, `FreeConstAliasId`, `InherentAssocConstId` and `TraitAssocConstId`, with no `UnevaluatedConst`. `value()` matches `ConstKind::Alias(_, AliasConst { kind, args, .. })` over the five kinds, with no `StaticId` arm, and `InherentSelf` is an error naming the const (L220-237). Both `FnDef` sites read `no_bound_vars().unwrap()` once (L741, L881). `fact.rs` builds its infcx with `TypingMode::Codegen` (R6). None of R1, R5, R8 (`AliasTyKind::def_id`), R9, R10, R12, R13, R14 or R15 has a site in `crates/hir-ty-share/`. `identity.rs` refuses `ConstKind::Alias` and `TyKind::Alias` as unnormalized rather than hashing them, so R7's `IsRigid` encoding does not arise.
> to window 190 (from fastcompile-ident-share, 19:45): `identity.rs` already has all three sites on disk.
>
> - L204 reads `let args = args.no_bound_vars().unwrap();`.
> - L328 reads `TyKind::Alias(_, alias)`.
> - L412 reads `ConstKind::Alias(..)`. There is no `ConstKind::Unevaluated` arm left.
>
> Don't apply them a second time: a second `no_bound_vars()` would not compile, because the args are no longer a `Binder`. I am leaving those three arms to you as asked, and not touching them.

## coordinator

Window `coord`. 5 sites in 1 files.


Besides the sites below:

- `crates/macros/src/lib.rs`, `generic_type_visitable_derive`: change `s.bound_impl(` to
  `s.unsafe_bound_impl(`, and wrap the result in `quote! { #[allow(unsafe_code)] #generated }` with
  the SAFETY comment of R11. Every `#[derive(GenericTypeVisitable)]` in hir-ty (lib.rs, mir.rs)
  depends on this.
- hir-ide `tests/builtin_derives.rs`: the expected strings print `TraitPredicate(`, and the new
  Debug output prints `TraitClause(` (R1). They are listed below with that rewrite. Re-bless the
  expect tests once the tree builds, to confirm the rest of each string did not change.
- `Cargo.lock` and blaze-wasm's own lock still name `ra-ap-rustc_type_ir_macros` at the old version.

> refused (regenerated list): every `display.rs` site is ported already (`TyKind::Alias(_, a)`, `Ty::new_alias(.., is_rigid, ..)`, `FnDef` args read once with `no_bound_vars().unwrap()`); the new suggestions add a third field `TyKind::Alias` does not have. `builtin_derives.rs` already prints `TraitClause(` (re-bless at the first compile). `crates/macros` and the locks are the coordinator's, listed in requests' report.
> applied (requests): `crates/hir-ide/src/display.rs` has no old name left (`TraitClause`, `ClausePolarity`, `OutlivesClause`, `TyKind::Alias(IsRigid, _)` with `write_projection` keeping the flag it matched, `ConstKind::Alias` over `AliasConstKind` with no `StaticId` arm, `FnDef` args through `no_bound_vars().unwrap()`); `crates/hir-ide/src/tests/builtin_derives.rs` has all 48 `value: TraitPredicate(` as `value: TraitClause(` (re-bless at the first build). > forwarded to the coordinator: `crates/macros/src/lib.rs` `generic_type_visitable_derive` (`unsafe_bound_impl`, wrapped in `#[allow(unsafe_code)]` with R11's SAFETY comment) is its crate, not `requests'`; I had written it and took it back out, so the file is as in HEAD. The two locks are the coordinator's too.


### `crates/hir-ide/src/display.rs`

- L706: `let TyKind::Alias(_, a) = ty.kind() else {`. Withdrawn: this line is already ported; my regenerated suggestion was wrong. Nothing to do.
- L716: `Either::Left(Ty::new_alias(f.interner, is_rigid, *alias)),`. Withdrawn: this line is already ported; my regenerated suggestion was wrong. Nothing to do.
- L1347: `TyKind::FnDef(def, args) => {` → `TyKind::FnDef(def, args) => {`. R4: pattern unchanged; read `args` as `args.no_bound_vars().unwrap()` (once, `let args = args.no_bound_vars().unwrap();`), or pass it on unchanged where it is put back into a `FnDef`.
- L1491: `TyKind::Alias(` → `TyKind::Alias(_,`
- L1501: `TyKind::Alias(_, alias_ty @ AliasTy { kind: AliasTyKind::Opaque { def_id }, .. }) => {`. Withdrawn: this line is already ported; my regenerated suggestion was wrong. Nothing to do.

## Added 2026-10-29 19:25 (new sites only; earlier lines unchanged)

> forwarded: each site to its section's owner, as above.

### fastcompile-ident-share

- `crates/hir-ty-share/src/walk.rs` L36: `next_solver::{AllocationData, Const, ConstKind, GenericArgs, Ty, TyKind, UnevaluatedConst},` → `next_solver::{AliasConst, AliasConstKind, AllocationData, Const, ConstKind, FreeConstAliasId, GenericArgs, InherentAssocConstId, TraitAssocConstId, Ty, TyKind},`. R7.
  > already ported (fastcompile-ident-share, 19:50): L36-39 import exactly this list, with no `UnevaluatedConst`, so nothing was applied.
- `crates/hir-ty-share/src/walk.rs` L217: `let ConstKind::Unevaluated(UnevaluatedConst { def, args }) = konst.kind() else {` → `let ConstKind::Alias(_, AliasConst { kind, args, .. }) = konst.kind() else {`. R7. Then the match below becomes `match kind { AliasConstKind::Projection { def_id: TraitAssocConstId(id) } | AliasConstKind::InherentImpl { def_id: InherentAssocConstId(id) } | AliasConstKind::Free { def_id: FreeConstAliasId(id) } => { let (id, args) = lookup_impl_const(&cx.infcx, cx.env.param_env, id, args); db.const_eval(id, args, Some(cx.env)) } AliasConstKind::Anon { def_id } => db.anon_const_eval(def_id.0, args, Some(cx.env)), AliasConstKind::InherentSelf { .. } => return Err(Unshared::ConstEval(format!("`{konst:?}`: an inherent const in Self form after type check"))) }`. The `StaticId` arm goes, as in hir-ty-codegen's `eval_const`, which this mirrors.
  > already ported (fastcompile-ident-share, 19:50), so nothing was applied. L220 is `let ConstKind::Alias(_, AliasConst { kind, args, .. }) = konst.kind() else {`. L224-237 is this match: three kinds go through `lookup_impl_const` and `const_eval`, `Anon` goes to `anon_const_eval(def_id.0, ..)`, and `InherentSelf` is the error. There is no `StaticId` arm.
- `crates/hir-ty-share/src/walk.rs` L867: `TyKind::FnDef(def, fn_args) => match def.0 {` → `TyKind::FnDef(def, fn_args) => match def.0 {`. R4: pattern unchanged; read `fn_args` as `fn_args.no_bound_vars().unwrap()` (once, `let fn_args = fn_args.no_bound_vars().unwrap();`), or pass it on unchanged where it is put back into a `FnDef`.
  > already ported (fastcompile-ident-share, 19:50), so nothing was applied. The site is now L880-881, `TyKind::FnDef(def, fn_args) => { let fn_args = fn_args.no_bound_vars().unwrap(); match def.0 {`. The other `FnDef` site in `callee()`, L740-741, reads `let args = args.no_bound_vars().unwrap();` once.

> forwarded to fastcompile-ident-share (window `fastcompile-ident-share-next2`, told by tmux): `crates/hir-ty-share/` is its.
