# Requests of `lints-port-syntax` (crates/blaze-lints/src/lints, 14 lint files)

## lints-driver (via lane `lints`): `check::Cx` methods beyond cx.md

- What: on `Cx<'q, 't>` in `crates/blaze-lints/src/check.rs`, beside cx.md's methods:

  ```rust
  /// The function / impl a syntax item of the tree is (old `Semantics::to_def`).
  pub(crate) fn fn_def(&self, f: ast::Fn<'t>) -> Option<hir::Function>;
  pub(crate) fn impl_def(&self, imp: ast::Impl<'t>) -> Option<hir::Impl>;
  /// The view of where `adt` is written, in whichever file (old `HasSource::source`).
  pub(crate) fn adt_source(&self, adt: hir::Adt) -> Option<ast::Adt<'q>>;
  /// The macro `call` calls (old `Semantics::resolve_macro_call`).
  pub(crate) fn macro_def(&self, call: ast::MacroCall<'t>) -> Option<hir::Macro>;
  /// The `macro_rules!` or `macro` item defining `mac`; `None` for a proc macro.
  pub(crate) fn macro_source(&self, mac: hir::Macro) -> Option<ast::Macro<'q>>;
  /// `name` resolved as a one-segment path in the scope at `at`
  /// (old `Semantics::scope(at)?.speculative_resolve(&path)`).
  pub(crate) fn resolve_name(&self, at: ast::Node<'t>, name: &str) -> Option<hir::PathResolution<'q>>;
  ```

  and `crate::check::item_range(node: ast::Node<'_>) -> TextRange` kept `pub(crate)` (today's,
  on a view); `cx.expansion()` as cx.md has it (`empty_enums` read the old field).
- Where used: `new_without_default` (`fn_def`, `impl_def`, `adt_source`; it runs in
  `item_lints`, no body), `format` (`macro_def`, `macro_source`, for `#[clippy::format_args]`),
  `useless_format` (`resolve_name`, for a captured `{x}` of `format!`), `empty_enums`
  (`item_range`, `expansion()`).
- Why: each replaces a `cx.sema()` call the lint made; cx.md has no `sema()` and no typed
  call that answers these. Without them the lints cannot give what they gave.

> told `lints` and `lints-driver` by tmux, 2026-10-29. lints-driver: yes to `fn_def`,
> `impl_def`, `adt_source`, `item_range`, `expansion()` (plus `cx.def(ast::Node) ->
> Option<hir::ModuleDef>`); `macro_def`, `macro_source`, `resolve_name` awaiting an answer.
> forwarded to lints (from requests, pass 16; told by tmux).
> applied by lints-driver: in `check.rs` and `lints/mod.rs` (checked by lane `lints`, 2026-10-29).

## lints-driver: `lints/mod.rs` call shapes

The per-kind functions keep their names and take the view by value:
`collapsible_if::check(cx, ast::IfExpr)`, `crate_in_macro_def::check(cx, ast::MacroRules)`,
`empty_enums::check(cx, ast::Enum)`, `nonstandard_macro_braces::{check_call(cx, ast::MacroCall),
check_rules(cx, ast::MacroRules)}`, `redundant_static_lifetimes::{check_const(cx, ast::Const),
check_static(cx, ast::Static)}`, `upper_case_acronyms::check(cx, ast::Item)`,
`new_without_default::check(cx, ast::Impl)`, `needless_bool::{check_if(cx, ast::IfExpr),
check_block(cx, ast::StmtList)}`, `needless_return::{check_fn(cx, ast::Fn),
check_closure(cx, ast::ClosureExpr)}`, `match_like_matches_macro::{check_match(cx,
ast::MatchExpr), check_if_let(cx, ast::IfExpr)}`, `bool_assert_comparison::check`,
`useless_format::check`, `useless_borrows_in_formatting::check` (each `ast::MacroCall`). All
take `cx: &mut Cx<'_, 't>`.

> forwarded to lints (from requests, pass 16; told by tmux).
> applied by lints-driver: in `check.rs` and `lints/mod.rs` (checked by lane `lints`, 2026-10-29).
