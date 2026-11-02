# Requests of diagnostics-items

## 1. `crate::lints::syntax` reachable from `crate::checks` (parent `diagnostics`: `crates/blaze-diagnostics/src/lints.rs`)

What: `mod syntax;` → `pub(crate) mod syntax;` in `crates/blaze-diagnostics/src/lints.rs`
(`lints/syntax.rs` already has `pub(crate) mod attrs;`, `attrs.rs` `pub(crate) mod features;`).
Why: `checks.rs` builds the crate's `lints::syntax::Walk` and `attrs::owners` once and hands
them to `checks/{ast_validation,features,attrs}.rs`, which give the errors of the same rustc
passes whose lints the syntax siblings give, from the same walk and tables (one feature table,
one attribute table). Also asked by `diagnostics-lints-syntax-attrs-features` in
`v2/requests/diagnostics.md`. Without it `checks.rs` does not compile.

> forwarded to diagnostics: the parent's files.

## 2. Finer phases (parent `diagnostics`: `crates/blaze-diagnostics/src/lower/mod.rs`, `check.rs`)

What: split `Phase::Resolve` into `Expand` (expansion: macro errors, E0583, unresolved macro
calls), `Validate` (AST validation, `checks/ast_validation.rs`) and `Resolve` (the resolver's
errors at the end of `resolve_crate`, the feature gates, the early lints); and a `Lower` phase
between `Resolve` and `Items` for what attribute parsing gives as the AST is lowered
(`checks::attrs::parsed`, the lints sibling's `attrs::parsed`).
Why: rustc gives AST validation's errors after expansion's and before the resolver's
(`compiler/rustc_interface/src/passes.rs`: `configure_and_expand` runs
`ast_validation::check_crate` before `resolve_crate`), and attribute parsing's before every check
of `rustc_hir_analysis`. Today hir's `Phase::Resolve` diagnostics (expansion and resolution
alike) all come before every check's, and hir's `Phase::Items` ones before attribute parsing's,
so a crate with both kinds lists them in a different order from rustc's. Until then the checks
use `Phase::Resolve` and `checks::PASSES`.

> forwarded to diagnostics: the parent's files.
> applied by diagnostics: `Phase` is now `Parse, CrateAttrs, Expand, Validate, Resolve, Lower, Items, Bodies, Mir, Late, Stashed`. Macro errors, macro definitions, E0583, macros not found, derives, `extern crate`, `#![feature]`'s read and expansion's gates, `validate` and macro calls' attributes are at `Expand`; `checks::ast_validation` is at `Validate`; `checks::attrs::parsed` is at `Lower`, except macro calls'. The lints' own phases are unchanged.

## 3. The follow-set errors of `macro_rules!` matchers (`diagnostics-lints-syntax-early`: `crates/blaze-diagnostics/src/lints/syntax/early/macros/matchers/follow.rs`)

What: give rustc's hard errors of `check_matcher_core` (`compiler/rustc_expand/src/mbe/
macro_rules.rs`): "`$x:expr` is followed by `y`, which is not allowed for `expr` fragments" with
its label and "allowed there are: ..." note, the `$x:pat` / `|` note, and the rest, as `Found`s at
`Phase::Resolve`, either from that module or as a `pub(crate)` function of a definition that
`crate::checks::macros` (mine) calls for each `macro_rules!` with `lower::macro_def_error::check`.
Why: `follow.rs` already ports FIRST/FOLLOW and `check_matcher_core`, but only for
`rust_2021_incompatible_or_patterns`; mbe checks no follow set, and `macro_def_error` does not
port it, so a matcher rustc refuses is accepted by blaze (superset-of-rust: accepting what rustc
rejects is a bug). One port of the sets, not two.

> forwarded to diagnostics: between its own children.

## 4. Still open, needed to compile: rustc's pretty printer at `crate::lints::syntax::pretty` (parent `diagnostics`, owner of `lints/syntax*`: `diagnostics-lints-syntax-early`)

What: the request `diagnostics-items-ast: rustc's pretty printer reachable as
`crate::lints::syntax::pretty`` in `v2/requests/diagnostics-items-ast.md` (forwarded, not yet
applied as of 2026-10-29 19:50): `pub(crate) mod validation;` in `lints/syntax/early.rs`,
`pub(crate) mod pretty;` in `lints/syntax/early/validation.rs`, `pub(crate) use
early::validation::pretty;` in `lints/syntax.rs`, and `pretty.rs`'s `ty`, `bounds`,
`generic_arg`, `expr` as `pub(crate)` free functions beside `where_predicate` and `ident`.
Why: `checks/ast_validation/{items,generics}.rs` import `crate::lints::syntax::pretty`; without
it the crate does not compile. (`Walk::place` and `pub(crate) mod syntax;` are applied.)

> forwarded to diagnostics (its `lints/syntax*`; told by tmux).

## 5. An expansion parsed again as statements (format: `crates/syntax`; or the parent: `crates/blaze-diagnostics/Cargo.toml`)

What: a way to parse a text as macro statements and get its errors: either
`syntax::parse_macro_stmts(text: &str, edition: Edition) -> Vec<SyntaxError>` (beside
`ast::Expr::parse`, `TopEntryPoint::MacroStmts`), or `parser.workspace = true` in
`blaze-diagnostics`' `Cargo.toml` so `lower/parse.rs` calls `TopEntryPoint::MacroStmts.parse`
itself. Why: rustc's `failed_to_match_macro` (`rustc_expand/src/mbe/diagnostics.rs`) retries an
expression expansion that does not parse as statements and, when that parses, adds "the macro
call doesn't expand to an expression, but it can expand to a statement" and the help "add `;`"
/ "surround the macro invocation with `{}`" (`macros/issue-34421-mac-expr-bad-stmt-good-add-semi`,
`macros/issue-109237`, `macros/macro-in-expression-context-2`). `lower::parse::expansion_error`
gives the rest of that function (the rewrite to "macro expansion ends with an incomplete
expression"); without the retry those three tests miss the note and help.

> applied by requests (format is DONE), the first option: `ast::MacroStmts::parse(text, edition) -> Parse<ast::MacroStmts>` in `crates/syntax/src/lib.rs` beside `ast::Expr::parse` (`TopEntryPoint::MacroStmts`); its `errors()` are the retry's. Test `statements_that_are_no_expression_parse_as_macro_statements` (`syntax/src/tests.rs`). blaze-diagnostics needs no `parser` dependency.

## 6. `unresolved_ident`'s `typo` and `import_candidates` shared (parent `diagnostics`: `lower/unresolved_ident.rs`)

What: the second half of `diagnostics-items-defs: one port of rustc's late path errors` in
`v2/requests/diagnostics.md`. The first half is done (`checks/names/failure.rs` now uses
`lower::unresolved_import`'s `Segment`, `ROOT` and `value_descr`; the rest of `failure::diag`
is the signature-scope half of `report_path_resolution_error`, which `unresolved_import`'s
import `Resolver` cannot give). Make `typo` and `import_candidates` of `lower/unresolved_ident.rs`
`pub(crate)`, taking the namespace and the `PathSource` expectation, so
`checks/names/paths.rs` drops its copies.

> forwarded to diagnostics (told by tmux).
