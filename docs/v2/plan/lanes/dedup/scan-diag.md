---
status: plan
summary: dedup-scan-diag (2026-10-29): one diagnostic model, Span, lint Level, CommandLine and rustc emitter, owned by blaze-diagnostics and used by blaze-lints; the blaze-lints copies deleted, the cases only blaze-lints had right ported, a proptest of the emitter against the deleted copy; what stays apart on purpose; speed of light.
date: 2026-10-29
---
# dedup-scan-diag: the diagnostics against blaze-lints

Part of [dedup](../dedup.md), under `dedup-scan`. Every edit is a request, because neither crate
is free: `v2/requests/diagnostics.md` and `v2/requests/coordinator.md`, both under
`## dedup: ...` headings naming `blaze-lints`. The reference emitter for the proptest is staged
at `v2/requests/dedup-scan-diag/emit_reference.rs`.

## diagnostics model against blaze-lints

### Owner: `blaze-diagnostics`

`blaze-diagnostics` depends on neither `blaze-lints` nor anything that does (only `blaze-engine`
and `blaze-cargo` depend on `blaze-lints`), so `blaze-lints` gains `blaze-diagnostics` and
loses `annotate-snippets`. rustc's emitter is the spec, and `blaze-diagnostics`' copy is the
better port of it: it follows a later rustc, measures files as rustc's `SourceFile` does (BOM and
`\r\n` taken out), writes error codes in titles, failure notes and `-Z ui-testing`, removes a
macro's recursive calls from the backtrace, labels only a call whose macro has a def site as
rustc does, writes the "originates in" note in the text and not the JSON as rustc does, and
borrows its text where the other copied it.

| primitive | copies | kept |
|---|---|---|
| `Span`, `Expansion`, `MacroKind`, `MultiSpan`, `SpanLabel`, `Sublevel`, `Child`, `Applicability`, `SuggestionStyle`, `Suggestion` | `blaze-diagnostics/src/model.rs`, `blaze-lints/src/diag.rs` | `model.rs`; `diag.rs` re-exports it |
| the diagnostic and its builders | `model::Diag`, `blaze_lints::diag::Diagnostic` | `Diag`, with `Diagnostic::is_fixable` moved in; a lint's level is decided before it is made a `Diag` at `Error` or `Warning`, as `blaze-diagnostics`' own lints are |
| text and JSON emitter | `blaze-diagnostics/src/emit{,/json}.rs`, `blaze-lints/src/emit.rs` | `blaze_diagnostics::emit`, plus `json_rendered` so `blaze-lints` renders once |
| "originates in" note and macro backtrace | `model::Diag::originates_in`, `emit::{backtrace, originates_in}`, `blaze-lints/src/check.rs` `originates_in` | `Span::macro_backtrace` and `Diag::origin_note`, the emitter's; the model's copy wrote `warning` for a note and kept recursive calls |
| lint `Level` | `blaze-diagnostics/src/lints/registry.rs`, `blaze-lints/src/level.rs` | `registry::Level` (perfect hash for attributes; `of_cap` is rustc's `Level::from_str`, which has no `force-warn`) |
| `CommandLine::parse` | `blaze-diagnostics/src/lints/levels.rs`, `blaze-lints/src/level.rs` | `lints::CommandLine` (rustc's error words, `--cap-lints` once, `--check-cfg`, `--cfg`, `--emit`) |
| `item_range`, `signature_range`, `derive_paths` | `blaze-diagnostics/src/cx.rs`, `blaze-lints/src/check.rs` | `cx`'s |

### Ported from `blaze-lints`

- A primary span given twice is one annotation: rustc's `MultiSpan::span_labels`, which
  `blaze-lints` went through and `emit::collect_annotations` did not.
- cargo's key order: when cargo takes the colours out of `rendered` it writes rustc's line again
  as `rendered`, every other key sorted at every depth, then `code`
  (`/tmp/cargo-src/src_compiler_mod.rs`, `CompilerMessage`). `blaze-lints` wrote that always; now
  it writes it only without colour, and rustc's line as it is with colour, as cargo does
  (`serve::cargo_order`).
- `is_fixable`.

### Fixed in both on the way

`def_site` was set only for proc macros in both `expansion_of`s, while the emitter treats a
missing one as rustc's dummy def site and writes no "in this macro invocation" label. rustc's
`ExpnData::def_site` is every macro's definition, so both now set it for `macro_rules!`, `macro`
and built-ins, which `blaze-diagnostics`' expectations of that label on a `macro_rules!` call
need. In
`blaze-lints` the old "has a def site" test of "in another crate's macro" becomes rustc's
`in_external_macro` rule: in an attribute or derive macro's expansion, always.

### Kept apart on purpose

- `blaze_api::{Diagnostic, Level, Span}`: the agent API's placed, versioned wire shape. It sits
  below `blaze-diagnostics`, and `api.rs` maps it with one `match`.
- `hir-ty-borrowck`'s `Sublevel`, `Applicability`, `Child`: a front-end crate below
  `blaze-diagnostics`, its spans not yet placed in files, lowered by `lower/borrowck_error.rs`.
- `blaze-manifest/src/diagnostic.rs`, `blaze-fmt/src/format_report_formatter.rs`: cargo's and
  rustfmt's own renderers, not rustc's.
- `ide-diagnostics`: rust-analyzer's IDE model.
- Not merged here: deciding a level and its notes (`blaze-lints/src/level.rs`'s `Levels`, `Target`,
  `explain` against `blaze-diagnostics/src/lints/levels.rs`'s builders and `Shared::explain`).
  The two read different lint stores (clippy's and blaze's groups against rustc's registry), so
  one copy needs one `Store` holding all three, which is a change to the lint registry and not a
  copy-paste merge. The notes they write agree on clippy's lints: for a default level rustc
  writes no group, because clippy's groups are externally loaded.
- `blaze-cargo/src/cli/json.rs` writes cargo's sorted form of `blaze_api::Diagnostic` with `code`
  among the sorted keys, where cargo puts it last. That is a third writer of this order, from
  the API type and not the model, so it belongs to the cargo crates (`dedup-scan-cargo`, lane
  `cargo`).

### Tests

- Moved: the backtrace tests go from `emit.rs` to `model.rs`. The two copies had no other
  tests of their own: `blaze-lints/src/tests.rs`'s whole-pipeline expectations, unchanged, now
  run on the shared emitter.
- Differential: `emit/reference_tests.rs` holds the deleted emitter (taking a `Diag`) as the
  reference, and a proptest compares them over ASCII `\n` files, one or two of them, 1-2 primary
  spans, labels with tabs, children with and without spans, every suggestion style and
  applicability, disjoint and attribute suggestions, short, colour and a narrow width.
  `written` must be equal byte for byte, `json` equal as values, and `json_rendered` equal to
  `json`.
- `Level`: a proptest of the perfect hash against the old string `match`, and
  `--cap-lints force-warn` refused.
- New: the note on a recursive macro, the note's level word, `is_fixable`, a primary span given
  twice, `cargo_order`, and `diag::place_diag`.

### Speed of light

Writing a diagnostic is an output path, not a hot one: a crate gives tens of diagnostics, and
each is one pass over the lines it shows. The bound is the bytes written. The shared emitter
borrows the file text and the diagnostic's strings: one `Fx` probe per file per diagnostic, a
binary search of the line starts per span, char counting only in non-ASCII files, and
`annotate-snippets` writing the result. That is rustc's own work less its per-line `String`s.
The deleted copy cloned the diagnostic's spans, children and suggestions and copied each
snippet's lines into a `String` first, so moving onto the shared one removes those copies
rather than adding cost. The two pieces left for `blaze-lints` to run once per diagnostic, a
clone for its note-once filter and `cargo_order`'s parse of one JSON line, are at the edge and
stay well under the cost of the rendering they follow.
