---
status: plan
date: 2026-10-30
summary: no-fallback in hir-def, hir-expand and mbe: each place that guessed and went on when name resolution, expansion or lowering could not do something, classified as error, rule or dead, with what it becomes.
---
# no-fallback: hir-def, hir-expand, mbe

Part of [front](../front.md). Kinds: **error** (a real fallback: it becomes a typed error the
build stops on), **rule** (rustc does the same; it stays, with the rustc function it matches),
**dead** (no build reads it). **recovery** is a rule of its own kind: an error value used only
after a diagnostic that stops the build was recorded, as rustc recovers. Lines are as of
2026-10-30, after this lane's edits.

## Errors, fixed

| file:line | kind | what it did | rustc | what it is now |
|---|---|---|---|---|
| `hir-def/src/visibility.rs:70`, `resolver.rs:359`, `nameres/path_resolution.rs:149`, `nameres/facts/unit.rs:761` | error | a `pub(in path)` or `pub(super)` whose path did not resolve, or named a non-module, gave `None` and every caller made the item `pub`; a path to a non-ancestor module was accepted | `rustc_resolve::build_reduced_graph` `try_resolve_visibility`: E0433, E0577, E0742, then `resolve_visibility` recovers with `Visibility::Public` | `Result<Visibility, VisibilityError>`, `VisibilityError { Unresolved, NotAModule { found }, NotAncestor { module }, Relative2018 }` with rustc's ancestor check; `Visibility::try_resolve`; the recovery is `VisibilityError::RECOVERED` (`pub`, as rustc). Test `visibility::view_tests::restricted_visibility_errors`. Reporting: the `hir::InvalidVisibility` row of this table |
| `hir-def/src/nameres/collector.rs:2681`, `facts/collect.rs:1008`, `assoc.rs:369` | error | an item-position (or impl/trait item) macro call past `recursion_limit` was dropped with `tracing::warn!`: its items silently missing | `rustc_expand::expand` `error_recursion_limit_reached` | `DefDiagnostic::recursion_overflow` (`MacroError` with `ExpandErrorKind::RecursionOverflow`, the macro's name as its path), lowered by blaze-diagnostics' `recursion_limit`. Test `nameres::tests::macros::macro_expansion_overflow_is_reported` |
| `hir-def/src/nameres/collector.rs:1014`, `facts/driver.rs:576`, `:1104` | error | past `FIXED_POINT_LIMIT` (8192) rounds that still made progress: `tracing::error!("name resolution is stuck")` and the def map as it was | none: rustc's loop has no bound | panic naming the crate (an internal limit, like rustc's `bug!`) |
| `hir-def/src/nameres/collector.rs:1767-1772`, `facts/unit.rs:976-979` (glob arms) | error | `use x::*` of a struct, function, constant or anything not a module, enum or trait bound nothing, with a `tracing::debug!` | `rustc_resolve` import resolution: E0432 unresolved import | `nameres::diagnostics::is_glob_of_non_module`, reported as `UnresolvedImport` by the collector's end pass and by `facts::driver::trait_item_import_diagnostics` (crate, block and delta passes). Test `nameres::tests::globs::glob_of_a_non_module_is_unresolved` |
| `hir-expand/src/name.rs:294`, `:333` | error | `x.<index>` whose text does not parse as `usize` (`0x1`, `1_0`, past `usize::MAX`) named field 0 | the field ident is the literal's text: E0609 no field | `tuple_index_name`: the text itself, unless plain decimal. Test `name::tests::tuple_index_names_its_text` |
| `hir-def/src/item_tree.rs:262` | error | an `ERROR` root gave an empty item tree in release (`never!`; debug panicked) | none | panic, as debug did |
| `hir-def/src/expr_store/lower.rs:1512`, `:2032`, `:3166` | error | `never!` then went on (a coroutine closure argument that is not a binding; a block `macro_rules!` its def map does not define) | none | `unreachable!` naming the invariant |
| `mbe/src/expander/matcher.rs:943` | error | `never!` on a metavariable expression in a matcher, then went on | `rustc_expand::mbe::quoted` refuses `${..}` in a matcher | `unreachable!`: `MetaTemplate::parse` refuses it in `Mode::Pattern` (`parser.rs:263`) |
| `mbe/src/expander/matcher.rs:985`, `:988`, `:1084`, `:1102` | error | `stdx::always!` matcher invariants logged and went on in release | none | `assert!` |
| `hir-def/src/expr_store/lower/generics.rs:60` | error | an `impl Trait` where it is not allowed (a field, a `let`, a generic default, a path's argument in a body) became `TypeRef::Error`: no E0562 | `rustc_ast_lowering` `lower_ty`: E0562 "`impl Trait` is not allowed in ..." | kept as `TypeRef::ImplTrait`; hir-ty lowers those positions `ImplTraitLoweringMode::Disallowed` and reports `ImplTraitNotAllowed` (queue 01283) |
| `hir-def/src/expr_store/lower.rs:3744` | error | a pattern type's pattern other than a range, an or-pattern or `!null` was `Pat::Missing`, which hir-ty reads as a parse error and leaves silent | the parser's "pattern not supported in pattern types" | `Pat::Wild`, reported by hir-ty's `lower_pattern_type` as `PatternTypeNotLowered { reason: Pattern }` (queue 01283) |
| `hir-def/src/nameres/collector.rs:1328`, `:1585`, `:3042`, `:3526`, `facts/collect.rs:1147`, `facts/unit.rs:837`, `hir/src/diagnostics.rs:2329` | error, now recovery | the collectors gave an item whose visibility errs `pub` and nothing reported it; a plain `pub(in m)` resolved from the current module in every edition | `try_resolve_visibility`: in 2015 a plain path is from the crate root, in 2018+ it is "relative paths are not supported in visibilities"; then `Visibility::Public` | `hir::InvalidVisibility { node, error }`, found by hir's walk over every item, assoc item, field, `use` and `extern crate` (`collect_visibilities`); the collectors' `pub` is recovery after it. `visibility_path` gives the 2015 rule and `VisibilityError::Relative2018`. Lowered by blaze-diagnostics (queue 01192). Tests `hir::tests::visibilities`, `visibility::view_tests::restricted_visibility_2015_is_from_the_root`. Gaps: rustc's E0578 (indeterminate path) is not told apart; the edition is the crate's, not the path's span's |
| `hir-def/src/nameres/collector.rs:1130`, `:1167`, `facts/collect.rs:890`, `:926` | error | a crate that is not `no_std` and has no `std` got `core`'s prelude; a prelude path that did not resolve was a `tracing::debug!` | `rustc_builtin_macros::standard_library_imports` injects `extern crate std` (`core` under `no_std`): E0463 can't find crate for `std`; the injected `use` fails with E0432 | `DefDiagnosticKind::Prelude { krate, error: PreludeError::{CrateNotFound, NoPreludeModule} }`, `hir::MissingPrelude`, lowered by blaze-diagnostics (queue 01192); no prelude. Test fixtures now say what they are: `hir-expand/src/fixture.rs:625` `sysroot_attr` marks a crate with no `std` dependency and no `#![no_std]`/`#![no_core]` `no_std` (it has `core`) or `no_core`. Tests `nameres::tests::prelude`. The playground graph (`blaze-wasm`) marks its crates the same way (queue 01194) |

## Errors, waiting on another lane

| file:line | kind | what it does | rustc | what it becomes |
|---|---|---|---|---|
| `hir-def/src/signatures.rs:1316`, `:1972`, `expr_store/lower.rs:1103` | recovery, not reported yet | an `extern "x"` whose string is no ABI is `ExternAbi::FALLBACK` (`extern "C"`); nothing reports it | `rustc_ast_lowering` `lower_abi`: E0703, then `ExternAbi::FALLBACK` | stays as rustc's recovery once blaze-diagnostics reports E0703 (queue 01175, no-fallback-tools) |
| `hir-def/src/hir.rs:275`, `:278` | recovery, reported in hir-def | an integer literal past `u128` lowers as `0` | `rustc_ast_lowering` `lower_lit` -> `report_lit_error` `IntTooLarge`, after cfg stripping (so not in syntax validation) | `ExprCollector::lower_literal` records `ExpressionStoreDiagnostics::IntLiteralTooLarge` for exprs and patterns (queue 01176). Waiting on the hir arm (01315, no-fallback-front-ty) and the blaze-diagnostics lowering (01316, no-fallback-tools) |
## Rules and recovery that stay

| file:line | kind | what it does | rustc function it matches |
|---|---|---|---|
| `hir-def/src/resolver.rs:580` | rule | `u16::from_le_bytes` with `u16` unresolved (or a module, after `use core::u16`) resolves the primitive | `rustc_resolve::late` `resolve_qpath`: `PathResult::Module`/`Failed` with a primitive first segment becomes `Res::PrimTy` with the rest unresolved |
| `hir-def/src/nameres/collector.rs:1064` (`reseed_with_unresolved_attribute`), `:791` | recovery | an attribute that resolves to no macro is reported (`UnresolvedMacroCall`) and the item collected without it | `rustc_resolve::macros` `smart_resolve_macro_path` reports, then `dummy_ext(MacroKind::Attr)`, a non-macro attribute that keeps the item |
| `hir-def/src/nameres/path_resolution.rs:350`, `:457` | rule | the extern prelude after the crate's own names | `rustc_resolve::ident` `early_resolve_ident_in_lexical_scope`, `Scope::ExternPrelude` |
| `hir-def/src/signatures.rs:1316`, `:1972` (no string) | rule | `extern fn` and `extern {}` with no string are `"C"` | `rustc_ast_lowering` `lower_extern`: `Extern::Implicit` is `ExternAbi::FALLBACK` (`C`) |
| `hir-def/src/signatures.rs:1326`, `expr_store/lower.rs:1106` | rule | a function or fn pointer with no `extern` is the Rust ABI | `ExternAbi::Rust` for `Extern::None` |
| `hir-def/src/expr_store/lower/path.rs:114`, `hir-expand/src/mod_path.rs:297`, `:362` | rule | `$crate` whose context has no macro definition is the local crate root | `rustc_resolve::Resolver::resolve_crate_root`: no outer expansion is `graph_root` |
| `hir-def/src/nameres.rs:1083` | rule | `recursion_limit` is 128 by default | `rustc_session` `Limit` default 128 |
| `hir-def/src/nameres/collector.rs:833` | recovery | an unparsable `#![recursion_limit]` keeps the default | rustc's attribute parser reports it; blaze-diagnostics does (`checks/attrs/parsers.rs:239`) |
| `hir-def/src/nameres/collector.rs:1836`, `facts/collect.rs:612`, `facts/unit.rs:1092`, `facts/macros.rs:1133` | rule | `use S as _` of a non-trait binds nothing | `rustc_resolve`: an underscore import only matters for traits |
| `hir-expand/src/declarative.rs:177`, `:208` | rule | `macro_rules!` is semi-opaque and `macro` opaque without `#[rustc_macro_transparency]` | `rustc_expand::mbe::macro_rules` `Transparency::fallback(macro_rules)` |
| `mbe/src/lib.rs:333` | rule | a macro whose rules all failed to parse is function-like | `rustc_expand::mbe::macro_rules` `compile_declarative_macro`: the dummy on error is a `LegacyBang` extension; its calls fail on `MacroDefinition` (`hir-expand/src/declarative.rs:39`) |
| `mbe/src/expander.rs:99`, `expander/model.rs:62` | recovery | no rule matches: the best partial match is transcribed with the error | rustc reports `failed_to_match_macro` and uses `DummyResult`; blaze reports `MacroError` and the build stops |
| `hir-expand/src/builtin/fn_macro.rs:1358` | recovery | `env!` of an unset variable reports and expands to a dummy string | `rustc_builtin_macros::env` `expand_env`: error, then `DummyResult::any` |
| `hir-expand/src/fixup.rs:404` | recovery | span repair of an attribute input that has syntax errors | input fixup runs only for input the parser already reported |
| `hir-def/src/expr_store/lower.rs` `Name::missing`, `missing_expr`, `unwrap_or_else(|| alloc_type_ref_desugared(UNIT))` (about 15 sites), `hir.rs:283-300` string/byte/char `value()` errors | recovery | a node the parser left out or an escape it rejected | rustc's parser recovery; the parse error (`syntax/src/validation.rs` unescape errors) stops the build |
| `hir-def/src/nameres/assoc.rs:244`, `:352` | recovery | an impl item macro that does not resolve is reported (`UnresolvedMacroCall`) | as `reseed_with_unresolved_attribute` |
| `hir-def/src/expr_store/lower.rs:4174` | recovery | `.await` outside an async context lowers with `Awaitable::No` and is reported | E0728 |

## Not fallbacks

- `hir-def/src/find_path.rs:466` and its fuel (`:447`): which path a diagnostic prints for an
  item; nothing a build compiles reads it.
- `hir-expand/src/files.rs:327-420`: the range a diagnostic points at when a node from a macro
  cannot be mapped up fully is the macro call's. rustc does the same for spans from an
  expansion (`Span::source_callsite`); only where an error is shown changes, never whether.
- `hir-def/src/nameres/mod_resolution.rs:59`: past 32 nested modules a `mod` declaration is
  reported as unresolved (`UnresolvedModule`). Not silent, but not rustc's rule either (rustc
  has no depth bound and reports circular modules by the file chain); to be replaced by that
  check.
- `hir-def/src/nameres/collector.rs:1817`, `facts/unit.rs:1068`: glob chains past 100 panic.
- `hir-def/src/deps/serve.rs:451`: an `include!` whose file is unknown counts as outside the
  source root, so the image is not shared: the safe side of a cache decision.
- `hir-expand/src/lib.rs:959`: an expander that reports nothing of what it read is
  `ExpansionReads::Unknown`: reruns, never reuse.
- `hir-expand/src/builtin/derive_macro/reference.rs` (`never!` on unions, `#[cfg(test)]`) and
  `mbe/src/expander/model/` (`never!`, `always!`, the model `crate::differential` compares the
  expander with): reference models for tests; no build expands through them.
- Poisoned-mutex `into_inner`, thread-local pools (`unwrap_or_default` on a pop), sorted-list
  lookups and `strip_prefix("r#").unwrap_or(text)`: true optionals.
- `hir-def/src/unstable_features.rs:88` `never_type_fallback`, `ExternAbi::FALLBACK` itself,
  `hir-expand/src/builtin/derive_macro.rs:642` (`fallback`: a derive's `_` arm),
  `attr_macro.rs:221`: Rust's own names, not fallbacks.
