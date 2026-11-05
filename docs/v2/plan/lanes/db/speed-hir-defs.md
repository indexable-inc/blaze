---
status: plan
summary: speed-hir-defs: hir-def's attributes (flags, value queries, docs) brought to the fast-code bar; signatures and interned locations in the two child notes.
date: 2026-10-29
---
# speed-hir-defs

Paths: hir-def's files outside the store, item tree and name resolution. `attrs.rs`,
`attrs/docs.rs`, `Cargo.toml`, `test_db.rs`, `macro_expansion_tests/` are here. Signatures are in
[speed-hir-defs-sigs.md](speed-hir-defs-sigs.md); interned locations, lang items, upvars and the
small files are in [speed-hir-defs-locs.md](speed-hir-defs-locs.md).

## Attributes (`attrs.rs`)

**Hot path.** `AttrFlags::query(owner)` runs once per item, field and generic parameter. The
collector, signature lowering, lang items, lints, codegen and `blaze-items`' item hashes all read
it. The value queries (`lang`, `repr`, `export_name`, `link_name`, `link_section`, `linkage`,
`target_feature`, ...) read one attribute's value.

**Benchmark: rustc's `rustc_attr_parsing`.** rustc parses each item's attributes once into
`hir::Attribute::Parsed(AttributeKind)`. Callers read them with `find_attr!`, which scans the
item's parsed list and matches on an enum tag. Attribute names are `Symbol`s interned by the
lexer, so `sym::inline` is a `u32` compare.

| | before | after | rustc |
|---|---|---|---|
| allocations per attribute walked | a `SmolStr` per path segment, on the heap past 23 bytes (`rustc_intrinsic_must_be_overridden`, `rustc_const_stable_indirect`, ...); `#[inline(..)]` 1–2 `String`s; `#[doc(..)]` a `SmolStr` per atom; `export_name`'s value | 0 (a `Cow` only for a string literal with escapes) | 0 |
| name compares per attribute | a `&str` `match` lowers to one `eq` per arm in order: up to 8 (key-value), 17 (token tree), 38 (word) arms, every arm for an unknown name | 1 constant-time hash of the length and 4 bytes, then 1.18 probed slots on average (at most 5) over the 65 known words, each a length test and a byte compare | 1 `u32` compare per arm of a `Symbol` match |
| syntax walks per item `blaze-items` hashes | 1 (flags) + 4 (`export_name`, `link_name`, `link_section`, `linkage`, each its own memo) + 1 per call of `is_rustc_nounwind` and `has_word` (not memoized; codegen's `fma.rs` asks per call site) | 1 (flags); every other answer is a bit test, unless the item has that attribute | 1 parse per item |
| memos per item `blaze-items` hashes | 5 | 1 | 1 |
| `symbol_attr` for a string literal | 2 source lookups, a resolver, an ast id map and a span map built | 1 source lookup, the literal read in place | read from the parsed attribute |
| bytes of flags | 8 per item; fields `Vec<Option<u64>>`, 16 per field | 16 per item (`u128`: 8 new gating flags, 56 bits free); fields `Box<[AttrFlags]>`, 16 per field, one allocation per variant | — |
| locks per read | none (salsa memo read) | none | none |

**What changed:**
- **Name dispatch.** `match_attr_flags` looks a name up in `WORD_INDEX`, a 256-byte
  open-addressed table built at compile time over the `Word` tag enum, then matches on the tag.
  Path segments are read as `&str` from the tokens (`name_text`, `up_to_two_segments`, `path_is`
  replace `AstPathExt`'s `SmolStr`s). The `#[doc(..)]` flags step through the token tree the
  way `DocAtom::parse` does, without building atoms. `#[inline(..)]` looks for the token
  `always`/`never`, not a substring of the text made into a `String`.
- **Gating flags.** `HAS_EXPORT_NAME`, `HAS_LINK_NAME`, `HAS_LINK_SECTION`, `HAS_LINKAGE`,
  `HAS_DIAGNOSTIC_ITEM`, `RUSTC_NOUNWIND`, `RUSTC_COMPTIME` and `PANIC_HANDLER` are set in the
  same walk, under exactly the condition the value query matches: a key-value or word attribute
  whose path is the one segment. The public functions keep their signatures. Each one tests
  its bit and only then asks its tracked query. `is_rustc_nounwind` is the bit.
  `has_word(name)` is the bit for those three words and a source walk for any other name.
- **`diagnostic_item`** is new: blaze-diagnostics' request. The per-item half is done; the
  crate-wide reverse map is not.
- **`symbol_attr`** finds the source once. It reads a string literal (in parentheses or not)
  directly, which is what `expand_attr_value` gives for one. Only a macro value builds the
  expander.
- **Field flags** are a dense slice indexed by `LocalFieldId`, not an `ArenaMap` of `Option`s.

**Outline bodies (#1294).** The flags walk already enters a lazy fn body only when its text may
start with `#`: format-parse put that check in `HasAttrs::inner_attributes_node`, so no flags
read parses a body. That check counts `//!` as a plain comment, though, so `docs` lost the inner
doc comments of an outline fn body. `attrs/docs.rs`'s `inner_docs_node` enters the body when its
text may start with `//!`, `/*!` or `#`, so a body with no inner docs still stays unparsed.

**Where it still doesn't match rustc, and what it would take.** rustc walks a parsed attribute
arena; we walk syntax cursors once per item. To match:
- Compute the flags while the item tree is lowered, from attributes stored compactly there
  (format/items). The item tree drops `inline`, `repr`, `doc`, `must_use`, `target_feature` and
  others today, and it has no entries for fields or generic parameters.
- Intern identifiers when lexing, so a name is a `u32` and dispatch is one table load.

Both are the format lane's layout. The flags' read cost (a 16-byte memo, one bit test) already
beats `find_attr!`'s scan for every flag-shaped attribute.

**Cold, left as is:** `docs`, `cfgs`, `fields_docs`, `fields_cfgs` (LRU'd, IDE and rustdoc);
`doc_aliases`, `doc_keyword`, `doc_html_root_url`, `derive_info`, `must_use_message`,
`define_opaque`, `native_libs`, `wasm_import_module` (each runs only for an item with its flag,
or for an `extern` block or crate root).

## Other files

- `Cargo.toml`: `blaze-format.workspace = true`. format-hir's request: the persist modules use
  it. `blaze-ids` was not added because nothing in hir-def uses it.
- `test_db.rs`: its `Mutex` event log and the `RunOrder` thread-local are test-only (the
  `TestDB` the unit tests run on), never on a build's path; left as is.
- `macro_expansion_tests/`: tests only; nothing on a build's path.

## Unsafe ([unsafe.md](../../design/unsafe.md))

- `attrs/docs.rs` `Docs::remove_indent` had 3 `unsafe` blocks (`as_mut_vec`, `as_bytes_mut`)
  so it could strip indents in place while the `String` was briefly not UTF-8. It buys no
  speed that matters: docs are cold, and the safe version doesn't allocate either. It takes the
  bytes out (`mem::take(..).into_bytes()`), moves the lines with `copy_within`, finds char
  starts by skipping continuation bytes, and ends with one `String::from_utf8` (one linear
  check). On a panic the guard leaves the docs empty and the map cleared, as before.
  `attrs/` has no `unsafe` left.
- `lib.rs`: the 4 lifetime transmutes (`BlockIdLt::to_static`, `BlockId::to_db`, and the same
  for `ModuleId`) are the only way to erase a salsa id's `'db`. They keep their `# Safety`
  sections and now have `// SAFETY:` comments too. Removing them is lane db's job (salsa ids
  without a lifetime); callers are in `hir`, `nameres/`, `visibility.rs` and codegen.
- `dyn_map.rs`: 2 transmutes over the `#[repr(transparent)]` `KeyMap`, with `// SAFETY:`
  (speed-hir-defs-locs).
- hir-def can't take `#![forbid(unsafe_code)]`: `nameres/` and the transmutes above still have
  `unsafe`. Each block holds one unsafe op (`multiple_unsafe_ops_per_block`).

## Tests (`attrs/tests.rs`, `attrs/docs.rs`)

- `every_word_is_found_at_its_tag`, `a_word_is_one_probe_on_average`,
  `other_names_are_no_word`: the index against a model (a linear search of `WORDS`), with near
  misses of every word.
- `the_word_index_flags_every_attribute_as_the_string_match_did`: the old string-`match` walk,
  kept verbatim in the test as the model. Every word goes through 19 attribute shapes (word,
  raw, key-value, token tree, 2- and 3-segment, tool paths, `unsafe(..)`), plus `#[doc(..)]`,
  `#[test]` and `#[inline(..)]` shapes. Expected: the same flags, and the gating flags exactly
  where `is1` matches.
- `gating_flags_of_known_shapes`.
- `gated_queries_read_what_the_source_says`: for every function of a fixture, each gated query
  must equal the ungated source read (`symbol_attr`, `has_word_in_source`). It covers
  `cfg_attr` on and off, a value in parentheses, `link_name` in an `extern` block, a 2-segment
  word and a non-string value.
- `field_flags_are_indexed_by_field` (a cfg'd-out field shifts the indices),
  `inner_docs_of_a_fn_body`, `a_block_may_start_with_inner_docs`.
- `remove_indent_as_by_lines`: `remove_indent` against a model that cuts each line with
  `str::char_indices` and pushes it onto a new string. It runs 2000 seeded docs of 1–6 lines:
  tabs, blank lines, 2-, 3- and 4-byte chars, and all three doc kinds.
