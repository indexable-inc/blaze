# Requests from `diagnostics-items-resolve`

## 1. `hir::MacroError` should carry the expansion error, not only its text (db-expand / hir)

Where: `crates/hir/src/diagnostics.rs` (`MacroError`, built in `emit_def_diagnostic` and
`collect_macro_call`), `crates/hir-expand/src/lib.rs` (`ExpandErrorKind`), `crates/mbe`.

What: give `MacroError` the `MacroCallId` and the `ExpandErrorKind` itself (or an enum of the kinds
blaze lowers: compile_error with its string, proc macro panic with its message, recursion overflow,
mbe match failure, definition with parse errors, other), instead of `message: String` and
`kind: &'static str`. For an mbe match failure, the matcher's account of the best failure as rustc's
`CollectTrackerAndEmitter` keeps it (`rustc_expand/src/mbe/diagnostics.rs`): the token it failed
at (or end of input), the remaining matcher's description and range (rustc's "while trying to match
meta-variable `$a:ident`" note) and whether adding a comma makes a rule match (the "missing comma
here" help).

Why: `lower/macro_error.rs` now tells the kind from the rendered text and the range, so a failure
whose span mbe puts on the open delimiter or the macro name gives "no rules of `m!` match this
call", not rustc's "no rules expected `x`"; and the note and comma help, which every rustc
`no rules expected` has, can't be given at all.

> forwarded to db: `hir` and `hir-expand` are lane db's.
> applied by db-expand: `hir_expand::ExpandErrorKind::Mbe` carries format's `mbe::ExpandErrorKind::MatchFailure` (re-exported as `hir_expand::{MbeErrorKind, MatchFailure}`): the failing token (`None` at the end of the input, rustc's "unexpected end of macro invocation"), the error's span at that token or the input's closing delimiter, `label()`, the remaining matcher and its span (the "while trying to match" note), and `missing_comma` (the "missing comma here" help). Test `a_call_no_rule_matches_carries_the_best_failure` in `crates/hir-def/src/macro_expansion_tests/builtin_errors.rs`. `hir::MacroError` carrying the kind is db-ty-next1's (`v2/requests/db-expand.md`).

## 2. Run `macro_def_error::check` on every local `macro_rules!` (diagnostics-items)

Where: `crates/blaze-diagnostics/src/lower/mod.rs` or the items checks.

What: call `lower::macro_def_error::check(cx, InFile<&ast::Macro>)` for each local `macro_rules!`
that hir does not report a `MacroDefError` for (or for all of them, and have
`AnyDiagnostic::MacroDefError` lower to nothing).

Run it from a salsa-tracked query keyed by the definition's `AstId`, so an unchanged definition
is never read again and a cache hit allocates nothing; `check` itself is one allocation-free
pass over the definition's tokens when there is no error.

Why: mbe accepts definitions rustc refuses: no rules at all (`macro_rules! f {}`), an invalid
fragment specifier (`$x:id`), duplicate bindings, empty repetitions; hir only reports what mbe
refuses, so those give no error today.

> forwarded to diagnostics: its own.

## 3. Builtin macros' errors in rustc's words (db-expand)

Where: `crates/hir-expand/src/builtin/fn_macro.rs`, `builtin/derive_macro.rs`.

What: the `ExpandError::other` texts of `env!`, `include!`, `concat!`, `format_args!`, the builtin
derives and the rest as `rustc_builtin_macros` gives them (e.g. "environment variable `X` not
defined at compile time" with its help).

Why: `lower/macro_error.rs` gives them as "blaze can't expand macro `x`: {text}", an error naming
what, but not rustc's.

> forwarded to db: `hir-expand`'s builtin macros are lane db's.
> applied by db-expand: `env!` (with `is_cargo_env_var`, the similar-variable help and `edit_distance` ported), `option_env!`, `include*!`, `line!`/`column!`, `concat!`, `concat_bytes!` and the `From`/`CoercePointee` derives give `rustc_builtin_macros`' texts, codes, notes and spans as `ExpandErrorKind::Builtin` (kind `builtin-macro-error`, or `builtin-derive-error` for derives); tests in `crates/hir-def/src/macro_expansion_tests/builtin_errors.rs`. Showing them without the "blaze can't expand" prefix waits on `lower/macro_error.rs` and `hir::MacroError` (requests in `v2/requests/db-expand.md`).

## 4. A parse error of a token stream in rustc's words (diagnostics-items / parse)

Where: `crates/blaze-diagnostics/src/lower/parse.rs`.

What: expose the translation of rust-analyzer's parser errors to rustc's (`parser_error`,
`expected_one`, `expected_item`) over a syntax tree that is not a file, so a macro expansion's
parse errors can use it.

Why: `lower/macro_expansion_parse_error.rs` gives rustc's "expected expression, found `x`" for an
expression, type or pattern and rust-analyzer's text for the rest; rustc's "macro expansion
ignores `x` and any tokens following" needs the fragment parse's leftovers, which rust-analyzer's
parser wraps in an `ERROR` node with no message.

> forwarded to diagnostics: its own (and format for `crates/parser`, if the parser's text changes).

## 5. Whether cargo invoked the compile (diagnostics / blaze-api)

Where: `crates/blaze-diagnostics` `Options`.

What: a flag for rustc's `was_invoked_from_cargo()`.

Why: E0432/E0433's "you might be missing a crate named `x`" help reads "if you wanted to use a
crate named `x`, use `cargo add x` to add it to your `Cargo.toml`" under cargo;
`lower/unresolved_import.rs` gives the non-cargo text.

> forwarded to diagnostics: `Options` is in `crates/blaze-diagnostics/src/check.rs`, its own; `blaze-api` needs no change (the daemon that sets the flag is lane cargo's).
> applied by diagnostics: `Cx::cargo()` reads the flag check-cfg already had (`CommandLine::check_cfg.cargo`, `CARGO_CRATE_NAME` set), so there is one source rather than a second `Options` field. `lower::unresolved_import::{missing_crate, missing_crate_2015}` give rustc's two `cargo add` texts and its two non-cargo ones, used by `unresolved_import.rs` and `checks/names/failure.rs`. Test: `tests/items/resolve.rs` `under_cargo_the_missing_crate_help_says_cargo_add`. Still missing: the 2015 case of a crate passed by `--extern` (rustc's "upgrade your edition" help and MachineApplicable).

## 6. `#![forbid(unsafe_code)]` on `blaze-diagnostics` (diagnostics)

Where: `crates/blaze-diagnostics/src/lib.rs`.

What: add `#![forbid(unsafe_code)]`.

Why: `docs/v2/plan/design/unsafe.md`: a crate with no unsafe module forbids it. The crate has
no `unsafe` of its own (`rg` finds it only inside test sources, as Rust text in strings), and
nothing in this lane needs it: the speed here comes from not running on a clean build and from
`macro_def_error::check`'s allocation-free single pass, not from unchecked access.

> forwarded to diagnostics: its own `src/lib.rs`.
