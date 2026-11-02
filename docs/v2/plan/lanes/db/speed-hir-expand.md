---
status: plan
summary: speed-hir-expand's pass over crates/hir-expand (names, paths, hygiene marks, macro inputs, eager expansion, attributes) against rustc_expand, rustc's builtin derives and its red-green graph; costs before and after, and what is still filed with format and the hir-def siblings.
date: 2026-10-29
---
# speed-hir-expand: hir-expand against rustc_expand

The builtin macros (`builtin.rs`, `builtin/`) are `speed-hir-expand-builtin`'s, with their own
notes in `speed-hir-expand-builtin.md`. The span map and the syntax-to-token-tree bridge are
format's (`crates/span`, `crates/syntax-bridge`); what hir-expand needs from them is filed in
`v2/requests/format.md` ("speed-hir-expand: span lookups per token ...").

## The benchmarks

- **rustc_expand.** A `TokenStream` is an `Lrc<Vec<TokenTree>>`: one allocation per stream, never
  one per token. A `Span` is 8 bytes (an inline `lo`/`len`/`ctxt` form, with an interned side
  table for spans that don't fit). A `macro_rules` transcription marks each token's span with
  `apply_mark`, but rustc caches marks in `HygieneData` behind one lock per session
  (single-threaded front end). Names are interned `Symbol`s (4 bytes); paths are `ThinVec`s of
  segments, one allocation per path.
- **rustc's builtin derives** (`rustc_builtin_macros::deriving`) read the item's AST and build
  the output AST directly: no text and no reparse.
- **Invalidation**: rustc's red-green graph fingerprints every query result (a stable 128-bit
  hash), so a recomputed result that hashes the same turns its readers green.

## Before and after, per thing a build does

| what | before | after |
|---|---|---|
| `Name` | 8 bytes, `Clone` only; `tt::Ident -> Name` re-interned the text (a hash and a probe) | 8 bytes, `Copy`; reuses the ident's symbol |
| `ModPath` per conversion | a `SmallVec<[Name; 1]>`: one heap allocation per path of 2+ segments, then `Interned::new` (hash, probe, and a free on a hit) | segments gathered on the stack (`SmallVec<[Name; 8]>`), interned once per distinct list: no allocation on a hit; 16 bytes, `Copy`; eq and hash are two words |
| hygiene mark per transcribed `macro_rules` token | `apply_mark`: up to 3 interned lookups and 3 `SyntaxContext::new` interns (each a salsa interner shard lock today, db-salsa S2) | once per distinct context per expansion (a 4-way cache local to the expansion); nearly every token carries one of 1 or 2 contexts |
| attribute-macro and derive input | `fixup_syntax` over the whole item (a dozen ast casts per node, two hash maps), error or not | only when a syntax error touches the item (one binary search over the file's sorted error ranges); error-free input reaches the macro as rustc gives it |
| eager input with no nested macro (`format_args!("{}", a)`) | a `SyntaxEditor` (factory, maps) and an `apply_edits` pass | none |
| eager input's nested lazy call | a `Parse` clone (two `Arc` increments on shared lines) | the root node |
| attribute path check (`is1`, item-tree filter, cfg stripping) | a `SmolStr` per attribute | the segment's text read in place |
| span-map entry (format's) | 24 bytes, `(TextSize, Span)` interleaved: the search read 24-byte entries | 4-byte `ends` column searched, 20-byte `spans` column loaded once: 24 bytes per entry, 16 offsets per cache line |
| span lookups per token, syntax to token tree | one binary search per token, `O(log n)` | two compares while the token stays in its piece or moves to the next (`span_map.rs` `InOrder` over speed-span-ast's `span_at_from` / `span_for_range_from`), at every macro argument, attribute and derive input, `macro_rules` definition, eager input and `include!` |
| down-mapping (IDE, speed-span-ast's) | linear scan of the map per token | a binary search of a lazily built anchor-sorted index, then the matching run |

Allocations per token on the expansion paths in my files are now zero: the mark cache is a
fixed array in a `Cell`, names and paths intern without allocating on a hit, and the per-token
callback of `cfg_process.rs` returns an empty `Vec` (no allocation) except where it inserts
`#`, `!`, `[` or `]`. What allocates per token is inside the bridge and the token tree builder
(format's).

## Cutoff through eviction

`parse_macro_expansion` is `lru = 512`. Under db-salsa's fingerprints an evicted expansion that
recomputes to the same value backdates, so its readers don't rerun. That needs `Hash` on its
output, `ExpandResult<(Parse<SyntaxNode>, ExpansionSpanMap)>`:

- `ExpansionSpanMap` derives `Hash`: one pass over its columns, no `Debug` formatting.
- `ExpandError` derives it.
- `syntax::Parse<T>` and `mbe::ValueResult` don't yet; both are format's, filed by db-expand
  (`v2/requests/db-expand.md`). The cheapest correct `Parse` hash is the green tree's content
  hash taken once when the tree is built, not a walk per fingerprint.

Hashes are by interned pointer (`Symbol`, `InternedSlice`, `SyntaxContext` ids). Those are
stable within a process and never reused, which is all a fingerprint between revisions needs.
They are not stable across runs, so nothing here may go into a persisted key.

## Parallel rounds

Expansions run as plane tasks through `run_all`. On their read paths in my files:
- the mark cache is per expansion, with no shared write;
- `inert_attr_macro.rs`'s `OnceLock<FxHashMap<Symbol, usize>>` is built once, then each lookup
  is one acquire load and a probe by symbol pointer;
- `syntax_error_ranges` is a tracked query, computed once per file and read by reference.

What remains is the interners' own writes on a miss, and the salsa interner's lock on a hit,
which db-salsa's S2 removes.

## Not yet matched, and why

- **Spans**: 20 bytes against rustc's 8. The `Span` type is format's (`crates/span`), and the
  token tree stores it per token.
- **Per-token lookups** go through `InOrder` everywhere tokens are walked in order in my paths.
  The builtin derives' own small conversions (`derive_macro.rs`) and the IDE's speculative
  expansion still take the plain lookup.
- **`Interned<ModPath>`**: hir-def still interns `ModPath` a second time at every store. That
  is filed with the hir-def siblings in `v2/requests/speed-hir.md`, and
  `hir_expand::attrs::Attr::path` moves to `ModPath` in the same change.
- **Proc macro calls**: build a `String` of the crate's working directory per call. The
  `expand_in` signature is `usercode-macros-native-next1`'s.
- **One parse per derive, not per `#[derive]`.** Each builtin derive of `#[derive(A, B, C)]`
  turns the same input tree back into syntax (`derive_macro.rs` `to_adt_syntax`), which is N-1
  extra parses and span maps per attribute. rustc reads the AST it already has. It can't read
  the source AST here, because the input is the item after `cfg` processing. What it would take:
  a tracked `MacroCallId::derive_input_syntax` (`returns(ref)`, `no_eq`) on the `#[derive]`
  attribute's call, parsing its `macro_arg` once, read by the derives. That needs the derive's
  id threaded into `BuiltinDeriveExpander::expander()`'s `fn(db, Span, &TopSubtree)` (a public
  signature held fixed in this pass), and `expand_speculative` keeping today's path for its own
  input tree.
