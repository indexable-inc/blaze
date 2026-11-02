# Requests of db-unsafeck

Status (19:55): 1 is in the root `Cargo.toml`. db-ty-next1 applied 2. 3 to 6 are open.

## 1. Coordinator: the root dependency `hir-ty-unsafeck` (db's request 9)

What: in the root `Cargo.toml` `[workspace.dependencies]`, beside `hir-ty-borrowck`:
`hir-ty-unsafeck = { path = "crates/hir-ty-unsafeck", version = "0.0.0" }`.
Why: `crates/hir/Cargo.toml` now has `hir-ty-unsafeck.workspace = true`, and the manifest lint
reports "`hir-ty-unsafeck` uses `workspace = true` but the root doesn't declare it" until this
lands. This is `v2/requests/db.md` item 9.

> applied by requests: `hir-ty-unsafeck` (and `hir-ty-matchck`) beside `hir-ty-borrowck`.

## 2. db-ty-next1: `hir` takes the unsafety check from `hir_ty_unsafeck`

What, in files of `crates/hir/` that are not db-unsafeck's:
- `crates/hir/src/lib.rs` (about line 211): drop `diagnostics::UnsafetyReason` from the
  `hir_ide::{..}` re-export. It is an explicit re-export, so it shadows the glob
  `diagnostics::*` that brings `hir_ty_unsafeck::UnsafetyReason` (re-exported from
  `crates/hir/src/diagnostics.rs`). Left in place, `hir::UnsafetyReason` is the old enum, which
  does not type-check against `MissingUnsafe::reason`.
- `crates/hir/src/semantics.rs:36` and `crates/hir/src/source_analyzer.rs:39-41`: import
  `unsafe_operations` and `InsideUnsafeBlock` from `hir_ty_unsafeck` instead of
  `hir_ide::diagnostics`. The signatures are the same (`db`, `infer`, `ExpressionStoreOwnerId`,
  `&ExpressionStore`, `ExprId`, `&mut dyn FnMut(ExprOrPatId, InsideUnsafeBlock)`).
Why: hir-ide's checker is deleted at integration, and the editor's highlighting should come
from the same walk as the diagnostics. Told in tmux window `db-ty-next1`.

> forwarded to db (window `db-next2`, told by tmux).
> checked by db-audit-dbsub (2026-10-29 22:33): in the tree at `crates/hir/src/semantics.rs:42` and `crates/hir/src/source_analyzer.rs:51` (`use hir_ty_unsafeck::..`); `crates/hir/src/lib.rs` no longer re-exports `diagnostics::UnsafetyReason`, and `crates/hir/src/diagnostics.rs:743` re-exports `hir_ty_unsafeck::{UnsafeLint, UnsafetyReason}`.

## 3. diagnostics: `MissingUnsafe` and the new reasons

`hir::MissingUnsafe` is now:

```rust
pub struct MissingUnsafe {
    pub node: InFile<ExprOrPatPtr>,
    pub lint: UnsafeLint,      // HardError | UnsafeOpInUnsafeFn | DeprecatedSafe2024
    pub reason: UnsafetyReason,
    pub first: bool,           // rustc's `suggest_unsafe_block`: the first lint of its body or closure
    pub in_unsafe_fn: bool,    // in an `unsafe fn` body, outside a block
}
```

`UnsafetyReason` (a `u8` enum, `hir_ty_unsafeck::UnsafetyReason` re-exported by `hir`) has
these variants beyond the six that `crates/blaze-diagnostics/src/lower/missing_unsafe.rs`
`Op::of` matches, each with rustc's `UnsafeOpKind` message:
- `UseOfUnsafeField`: "use of unsafe field";
- `InitializingTypeWithUnsafeField`: "initializing type with an unsafe field";
- `CallDropExplicitly`: `Drop::drop` or `Drop::pin_drop` called by path where type checking
  did not already refuse it, with the wording of rustc's
  `tests/ui/pin-ergonomics/pinned-drop-unsafety-check.stderr`;
- `InitializingTypeWith`: "initializing type with `rustc_layout_scalar_valid_range` attr";
- `MutationOfLayoutConstrainedField`: "mutation of layout constrained field";
- `BorrowOfLayoutConstrainedField`: "borrow of layout constrained field with interior
  mutability".

What: `Op::of` covers them. `first` replaces the `get_unsafe_ops` scan, so the lowering no
longer needs to be O(ops²). `crates/ide-diagnostics/src/handlers/missing_unsafe.rs`
`display_unsafety_reason` is exhaustive over the same enum and needs the same arms while that
crate exists. Told in tmux window `diagnostics`.

> forwarded to diagnostics (told by tmux).

## 4. db-expand-next1 (hir-expand, hir-def): `wrap_binder!` and `unwrap_binder!`

What: lower the builtin macros `core::unsafe_binder::{wrap_binder, unwrap_binder}` (feature
`unsafe_binders`) to expressions of their own in hir-def's store (rustc's
`ExprKind::UnsafeBinderCast` with `UnsafeBinderCastKind::{Wrap, Unwrap}`), and infer them in
hir-ty. Today neither is a builtin macro in `crates/hir-expand`, and hir-def has no expression
for them.
Why: `v2/requests/diagnostics.md` "diagnostics-lints-mir-unsafe" item 1, `UnsafeBinderCast`.
rustc requires `unsafe` for both (`ExprKind::{Place,Value}UnwrapUnsafeBinder`,
`WrapUnsafeBinder`). When the expressions exist, `hir-ty-unsafeck` adds
`UnsafetyReason::UnsafeBinderCast` at them. Until then a program using them fails as an
unresolved macro, so blaze accepts nothing rustc rejects.

> forwarded to db (window `db-next2`, told by tmux).
> applied by db-expand: the hir-expand half, by db-expand-macros (2026-10-29): nothing to add. In core (read from the local nightly-2026-05-27 source; no rust-src of the pinned nightly-2026-10-27 is on this machine), core's `wrap_binder!`/`unwrap_binder!` are declarative `pub macro`s (`library/core/src/unsafe_binder.rs`) that expand to `builtin # wrap_binder(expr[, ty])` / `builtin # unwrap_binder(..)`, and hir-expand expands them already. The parse of that syntax (format: `parser` `builtin_expr`, `syntax`) and the store's `Expr::UnsafeBinderCast` (mem-format) are requested in `v2/requests/db-expand.md`, "`builtin # wrap_binder` / `builtin # unwrap_binder`".

## 5. db-ty (hir-ty inference): which paths resolved to a static

What: one bitset over `ExprId` in `InferenceResult`, `static_paths`, holding the path
expressions inference resolved to `ValueNs::StaticId`, with
`pub fn path_is_static(&self, expr: ExprId) -> bool`. Inference resolves every value path
already (`infer/path.rs`).
Why: `hir-ty-unsafeck`'s `check_static` re-resolves every path that is not a variant, an
associated item or a function, through the resolver: every use of a local. With the bitset, it
is one bit test (`docs/v2/plan/lanes/db/speed-of-light-unsafeck.md`, "The gap").

> forwarded to db (window `db-next2`, told by tmux).
> db-ty-next3 (2026-10-29 ~22:20): **not needed in hir-ty.** Inference already records what
> every value path resolved to, statics included: `InferenceResult::value_resolution(id:
> ExprOrPatId) -> Option<ValueNs>` (`infer.rs`, written by `resolve_value_path` in
> `infer/path.rs` for each path expression and pattern). So the change is in
> `hir-ty-unsafeck/src/lib.rs` `check_static` alone (db-unsafeck's): after the early returns,
> `let Some(ValueNs::StaticId(id)) = self.infer.value_resolution(current.into()) else { return };`
> in place of the resolver, its scope guard and the hygiene lookup (and the `resolver` field
> if nothing else reads it). A path inference could not resolve records nothing, so nothing is
> reported for it, as rustc reports no unsafety for an unresolved path.
> applied by db-audit-dbsub (2026-10-29 22:33): as db-ty-next3 wrote, in `crates/hir-ty-unsafeck/src/lib.rs` `check_static`: `self.infer.value_resolution(current.into())` in place of the resolver walk; the `resolver` field and the `HasResolver`/`ResolveValueResult`/`Resolver` imports go. `docs/v2/plan/lanes/db/speed-of-light-unsafeck.md` "The gap" becomes "Static paths". Not built.

## 6. Tools: `tools/v2/hot_lint.py` cannot read its `crates` field

What: `hot_lint.py`'s `Input.crates` is `list[str]`, which `typed_input._check` refuses
("has a type it does not read: list[str]"; it reads `tuple[str, ...]`). Make it
`tuple[str, ...]` (and its default `tuple(HOT)`).
Why: `hot_lint.py '{"root": ".", "crates": ["hir-ty-unsafeck"]}'`, as common.md gives it, fails
before linting anything. db-unsafeck ran `hot_lint.lint_file` directly on its files instead,
and they are clean.

> forwarded to the coordinator (`tools/`), listed in requests' report.
