---
status: plan
summary: dedup-sim-diag (2026-10-29): the copies tools/similar found touching blaze-diagnostics, blaze-lints, blaze-doc and blaze-fmt; where each shared primitive lives (stability from hir-def, written_range and peel_parens in syntax, stdx::list, AdtId::variants, blaze_phf::bytes_eq), what each copy becomes and its speed of light.
date: 2026-10-29
---
# dedup-sim-diag: copies around the diagnostics crates

Part of the dedup lane. The 87 groups `tools/similar` found touching blaze-diagnostics,
blaze-lints, blaze-doc or blaze-fmt; the full list, group by group, is in the lane report
(`/tmp/blaze-lanes/v2-dedup-sim-diag.md`). The model, `Level` and the emitter were moved by
dedup-scan-diag. The edits are requests to the owners (`v2/requests/*.md`, headings
`## dedup: …` from dedup-sim-diag), except `blaze_phf::bytes_eq`, which is in.

## Stability and deprecation: hir-def's parse, read through `hir`

`lints/names/stability` parsed `#[stable]`, `#[unstable]`, `#[deprecated]` and
`#[rustc_deprecated]` again, and had drifted from hir-def's parse (which is rustc's
`rustc_attr_parsing`): a `soft` unstable was refused, a feature name was checked with
`is_alphabetic` rather than as an identifier, `CURRENT_RUSTC_VERSION` was a hard-coded
version, `FORCE_UNSTABLE` was missing. The pass now reads `HasAttrs::{deprecation,
stability}` for definitions, hir-def's `own_deprecation`/`own_stability` for macros (rustc's
`SyntaxExtension` reads its own attributes only), and keeps only what is the pass's own: the
walk, the `allows_unstable` check and the messages.

Speed of light: one salsa read per definition named (the stability hir-def already computed
for name resolution), against a parse of the attributes of every definition on every path: the
parse goes, nothing is added.

## Where rustc's span of an item starts: `syntax::view::written_range`

Five copies (two in blaze-diagnostics, blaze-lints, hir-expand, hir-ty-mir) of "from the first
child that is not an attribute, a doc comment or trivia". The rule is written once over a
node's children as `(SyntaxKind, TextRange)` pairs, `written_range_of`, so the handle callers
and the view callers share it; `written_range(Node)` is the view form new code calls. Around
it in blaze-diagnostics: one `checks::defs::head_range` (rustc's `guess_head_span`, which
`missing_unsafe` copied and `unresolved_ident` copied without rustc's multi-line rule) and one
`cx::closure_decl_range` (a closure's `fn_decl_span`, three copies).

Speed of light: a scan of the node's direct children up to the first that is written, no
allocation; the copies did the same.

## `ast::Expr::peel_parens`

Six copies on views in blaze-lints, two on handles in blaze-diagnostics. One method on the view
`ast::Expr`; the handle copies go when frontend-tree moves those files to views. A loop over
`ParenExpr` children, no allocation.

## English lists: `stdx::list`

Thirteen copies of rustc's three list forms ("a, b, and c", "a, b, or c", "a, b and c"),
eleven in blaze-diagnostics and one each in blaze-doc and `misc.rs`'s `tokens_to_string`. One
`stdx::list::list(items, Join, quote)` over any `ExactSizeIterator` of `Display`, with the
copies' tests and a proptest against the slice-match form they used. `stdx` because it is a
small text helper used across crates (the lane's ownership table).

Speed of light: one `String`, written once, on an error path; the copies that collected a
`Vec<String>` to `join` allocated per item.

## `AdtId::variants`

Four copies (borrowck, wfcheck, hir-ty-share, blaze-diagnostics) of an ADT's variants as
`VariantId`s, in order. On the id in hir-def; one `enum_variants` read for an enum, none for a
struct or union, as every copy did.

## `blaze_phf::bytes_eq`

The const byte-slice `==` blaze-phf had for its tables is `pub`, with a proptest against `==`;
blaze-diagnostics' two `same` copies call it. A const fn, compiled into the tables it checks.

## Not copies

- `descr`/`plural`/`title` (group 1): three schemas, rustc's `def_descr`, rustdoc's item type
  names and blaze-intent's path keywords. (rustc's `def_descr` itself is rewritten in about
  15 places inside blaze-diagnostics, below the size this pass takes, one site at a time.)
- `Spacing` in blaze-fmt repeats `tt::Spacing`, but taking it adds `tt` and its graph to
  blaze-fmt for four lines.
- blaze-fmt's `peel_parens` and trivia walks are on its own rustc_ast port.
- `checks/features/library.rs` `name_values` is rustc's `lib_features` (every stability
  attribute with its range and `#[unstable_removed]`); `checks/attrs/parsers/stability.rs`
  gives rustc's malformed-attribute errors. hir-def's per-definition result keeps neither.
