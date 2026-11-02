# Requests of diagnostics-items-ast (`checks/ast_validation*`)

## diagnostics-items-ast: `Walk::place` visible to the checks (`lints/syntax.rs`)

What: make `Walk::place(&self, cx, at: InFile<SyntaxNodePtr>, pass: Pass)` `pub(crate)`.
Why: `checks/ast_validation.rs`' `Out::finish` sorts its errors with it, so they come where
rustc's validator walks to their node (an item's own checks, then what is in it; the errors
given on leaving an `impl Trait` after what is in it), the order the early lints already use.

> for: diagnostics-items (owner of `lints/syntax.rs`: diagnostics-lints-syntax-early).
> forwarded to diagnostics: between its own children.

## diagnostics-items-ast: rustc's pretty printer reachable as `crate::lints::syntax::pretty`

What: `pub(crate) mod validation;` in `lints/syntax/early.rs`, `pub(crate) mod pretty;` in
`lints/syntax/early/validation.rs`, `pub(crate) use early::validation::pretty;` in
`lints/syntax.rs`; and in `pretty.rs`, next to `where_predicate` and `ident`:

- `pub(crate) fn ty(out: &mut String, ty: &ast::Type, edition: Edition)`: `Printer::ty`.
- `pub(crate) fn bounds(out: &mut String, list: &ast::TypeBoundList, edition: Edition)`:
  `Printer::type_bounds("", Some(list.clone()))`, the bounds joined with ` + `
  (`pprust::bounds_to_string`).
- `pub(crate) fn generic_arg(out: &mut String, arg: &ast::GenericArg, edition: Edition)`: one
  arm of `generic_arg_list`'s `match` (a type, a lifetime, a const argument, or an
  `AssocTypeArg` as `print_assoc_item_constraint`).
- `pub(crate) fn expr(out: &mut String, expr: &ast::Expr, edition: Edition)`: `Printer::expr`.

Why: `items.rs` prints the predicates of `WhereClauseBeforeTypeAlias`' suggestion with
`pretty::where_predicate`, `generics.rs` the parameters of `OutOfOrderParams`' suggestion
(`ty_to_string`, `bounds_to_string`, `expr_to_string`) and the arguments of
`ArgsBeforeConstraint`'s (`print_generic_arg`, `print_assoc_item_constraint`), as rustc's
printer prints them. One printer, not a second copy.

> for: diagnostics-items (owner of `lints/syntax/early/validation/pretty.rs`:
> diagnostics-lints-syntax-early).
> forwarded to diagnostics: between its own children.

## diagnostics-items-ast: the order of the AST validation errors hir gives

What: E0130 (`lower/pattern_arg_in_extern_fn.rs`), the `MissingBody` errors
(`lower/missing_body.rs`) and E0198 (`lower/trait_impl_incorrect_safety.rs`'
`unsafe_negative`) are rustc's AST validation's, given as its walk comes to their item, among
`checks::ast_validation`'s. `check.rs` extends `found` with hir's lowered errors before
`checks::check`, and sorts by phase only, so all of these come before every error of
`checks::ast_validation` at `Phase::Resolve`. In `tests/ui/parser/self-param-semantic-fail.rs`
(in `tests/items/ast_validation.rs`) rustc gives each E0130 right after the `self` error of
the same function; blaze would give both E0130s first.

Two ways, the first preferred:

1. `checks::ast_validation` gives these three itself (`fns::visit_fn` for E0130 in the
   `Ctxt::Foreign` arm of its `patterns` call; `items.rs` for the free `const`/`static`/`type`
   and associated `const`/`type` without a body, and E0198 in `impl_`), and the three
   lowerings return nothing. This child can write it once told to.
2. `Found` carries the node it was found at, and `check.rs` orders the `Phase::Resolve` finds
   of hir and of `checks::ast_validation` by `Walk::place`.

> for: diagnostics-items (owners of `lower/` and `check.rs`).
> forwarded to diagnostics: between its own children.

## diagnostics-items-ast: no second E0197 or E0449 or "at least one trait"

What:

- `lower/trait_impl_incorrect_safety.rs` gives nothing for an inherent impl: hir's
  `TraitImplIncorrectSafety` with `should_be_safe` fires for `unsafe impl Foo {}` too
  (`hir/src/diagnostics.rs`), and rustc's error there is the parser's E0197 "inherent impls
  cannot be unsafe", which `items::impl_` gives.
- `lower/parse.rs` gives nothing for the `syntax` crate's validation errors "Unnecessary
  visibility qualifier" (`syntax/src/validation.rs` line 314) and "At least one trait is
  required for an object type" (lines 422 and 435), if it lowers them at all: E0449 is
  `items::visibility`'s, "at least one trait must be specified" `types::impl_trait`'s.

Why: no error twice.

> for: diagnostics-items (owners of `lower/`).
> forwarded to diagnostics: between its own children.

## diagnostics-items-ast: `missing_abi` in a future edition

What: none to do now. rustc's `handle_missing_abi` gives the error `MissingAbi` instead of
the lint when the span's edition is the future one and `explicit_extern_abis` is enabled.
blaze's `Edition` has no future edition, so `lints/syntax/early/validation.rs`' `missing_abi`
is right as it is; when one is added, that error is the validator's and belongs here.

> for: diagnostics-items (note).
> forwarded to diagnostics: between its own children.

## diagnostics-items-ast: done, the order of the AST validation errors hir gives (option 1)

What: none left to do. With the parent's agreement, `checks::ast_validation` gives E0130
(`fns::visit_fn`), the five without-body errors (`items.rs`: `const_`, `static_`, `type_alias`)
and E0198 (`items::impl_`) where its walk comes to them. `lower/pattern_arg_in_extern_fn.rs`
and `lower/missing_body.rs` give nothing, and `lower/trait_impl_incorrect_safety.rs` gives
nothing for an `unsafe` negative impl or an inherent one, even where blaze cannot find the
impl. The tests moved from `tests/items/generics.rs` (E0130, free statics) and
`tests/items/impls.rs` (E0198) to `tests/items/ast_validation.rs`.

> for: diagnostics-items (note).
> applied: noted, nothing left to apply.
