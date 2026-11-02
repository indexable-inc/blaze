# Requests of lints-more-types (crates/blaze-lints, child of lane `lints`)

## db (`crates/hir`): five type questions clippy's `needless_pass_by_value` and `multiple_unsafe_ops_per_block` ask

- What, in `crates/hir/src/lib.rs`:
  - `impl<'db> Type<'db> { pub fn env_bounds(&self, db: &'db Db) -> Vec<(Trait, Vec<Type<'db>>)> }`: the trait
    clauses of the type's param env, elaborated (`traits::elaborate`), not global, other than `Sized` and
    `MetaSized`, whose self type is exactly `self` (no autoderef, unlike `env_traits`), each with its generic
    arguments after `Self`. clippy: `traits::elaborate(cx.param_env.caller_bounds())...filter(|t| t.self_ty() == ty)`.
  - `pub fn ref_to(&self, db: &'db Db, mutability: Mutability) -> Type<'db>`: `&'erased self` in the same env
    (clippy's `Ty::new_imm_ref(tcx, re_erased, ty)`), so `impls_trait` can be asked of it.
  - `pub fn is_sized(&self, db: &'db Db) -> bool`: `ty.is_sized(tcx, typing_env)` in the type's env.
  - `pub fn may_implement_copy(&self, db: &'db Db) -> bool`: rustc's
    `type_allowed_to_implement_copy(..).is_ok()`: an ADT with no `Drop` impl whose every field, with the
    type's arguments, is `Copy` in the env.
  - `impl Callable<'_> { pub fn safety(&self) -> hir_ty::next_solver::abi::Safety }`: the signature's
    safety, for an `unsafe fn` pointer callee (`ty::FnPtr(sig_tys, hdr) => sig.safety()`).
- Why: blaze-lints ports clippy's `needless_pass_by_value` (the workspace denies it) and
  `multiple_unsafe_ops_per_block`; they ask exactly these of rustc's trait system, and hir has
  no public way to ask them. Semantics belong in the front end (semantics-in-front-end), so the
  lints ask hir rather than rebuilding the solver's answers.

> forwarded to db (`crates/hir`) (from requests, pass 16; told by tmux).
> applied by db-audit-other-types (2026-10-29 22:41): none of the five existed; written in
> `crates/hir/src/lib.rs` with the callers' signatures: `Type::env_bounds` (beside `env_traits`;
> type arguments only after `Self`, as `impls_trait` takes types), `Type::ref_to` (erased region,
> beside `add_reference`), `Type::is_sized` (`Sized` via `implements_trait_unique`, an error type
> sized), `Type::may_implement_copy` (`type_allowed_to_implement_copy` with `Safety::Safe`: core's
> primitives, else an ADT with no unsafe field, no `drop::destructor`, every field `is_copy` with the
> type's arguments; modulo regions, hir-ty has no region resolver outside bodies), and
> `Callable::safety` (the erased signature's). Tests: `crates/hir/src/tests/types.rs`.
> checked by db-audit-other (22:52): in the tree at `crates/hir/src/lib.rs`: `ref_to` :5174, `is_sized` :5468, `may_implement_copy` :5489 (rustc's arms and its skip of error fields), `env_bounds` :6204, `Callable::safety` :6485. Not compiled.

## db (`crates/hir`): two more for `needless_pass_by_value`

- What:
  - in `crates/hir/src/lib.rs`, `impl<'db> Type<'db> { pub fn impls_trait_fresh(&self, db: &'db Db,
    trait_: Trait) -> bool }`: whether `self` implements `trait_` for some arguments after `Self`,
    each a fresh inference variable (`traits::implements_trait_unique(self.ty, db, self.param_env,
    trait_.id)` with the rest of the args fresh). clippy asks it of `RangeBounds<?T>`
    (`implements_trait(cx, ty, range_bounds, &[])` leaves the arg to inference); `impls_trait` with
    no args makes them error types, which never hold.
  - on `hir::BodyAnalysis` (the lints lane's request, `crates/hir/src/body_analysis.rs`), `pub fn
    consumes(&self, local: Local<'db>) -> bool`: whether the body moves out of a place whose base is
    `local`: rustc's `ExprUseVisitor::for_clippy(..).consume_body(body)` with a delegate that records
    `consume` of a non-`Copy` place based on a local (clippy's `MovedVariablesCtxt`). hir-ty's port
    of the visitor (`hir-ty/src/infer/closure/analysis/expr_use_visitor.rs`) is crate-private and
    runs inside inference; this needs an entry over the finished `InferenceResult`.
- Why: without them `needless_pass_by_value` can't tell a consumed argument from one that is only
  read (it would lint every moved `Vec`), nor skip `RangeBounds` arguments as clippy does. Which uses
  move is semantics; it belongs in the front end (semantics-in-front-end), not re-derived in a lint.

> forwarded to db (db-next2, `crates/hir/`) (requests, pass 18; told by tmux).
> applied by db-audit-other-types (2026-10-29 22:41): the `impls_trait_fresh` half only, in
> `crates/hir/src/lib.rs` after `impls_trait`: `traits::implements_trait_unique`, which already
> fills the arguments after `Self` fresh; tested in `crates/hir/src/tests/types.rs`. The
> `BodyAnalysis::consumes` half is `db-audit-other-bodyanalysis`'.
> applied by db-audit-other-bodyanalysis (2026-10-29 23:10): the `consumes` half only.
> `hir_ty::infer::closure::analysis::expr_use_visitor::moved_locals(db, def, &InferenceResult)`
> merges the finished result into a fresh context and walks the body with
> `ExprUseVisitor::for_body` (rustc's `body_owner_is_closure == false`: a nested closure's
> captures are the body's locals) and clippy's `MovedVariablesCtxt` as delegate; `consumes`
> binary-searches its sorted answer, `true` for a body the walk stops at a type error in.
> Tested in `crates/hir/src/body_analysis/tests.rs` (moved, read, `move` closure); not built.
> checked by db-audit-other (23:16): both halves are in the tree. `impls_trait_fresh` is at `crates/hir/src/lib.rs:5363`. `consumes` is at `crates/hir/src/body_analysis.rs:351`, over `hir_ty::infer::closure::analysis::expr_use_visitor::moved_locals` (sorted and deduped; only `consume` is recorded, as clippy's `MovedVariablesCtxt`). Not compiled.

## lints-driver (`crates/blaze-lints/src/check.rs`, `lints/mod.rs`)

Told by tmux, with the whole list in `/tmp/blaze-lanes/lints-more-types-to-driver.md`: the 12
lints in `implemented!`, `On` widened to `u64`, `mod more_types_tests;`, the calls from the
walks, and six `Cx` additions (`path` at item level, `resolve_type`, `fn_def`/`adt_def`,
`is_exported`, `is_in_test`, `def_span`).

> forwarded to lints (from requests, pass 16; told by tmux).
> applied by lints-driver: in `check.rs` and `lints/mod.rs` (checked by lane `lints`, 2026-10-29).
