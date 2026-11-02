---
status: plan
summary: speed-span-tt — tt's storage as flat columns (8-byte nodes, CSR subtrees, packed spans in a side column, canonical so equality is a memcmp) and syntax-bridge as one preallocated pass with no String per token; benchmarks, before, changes, after.
date: 2026-10-29
---
# speed-span-tt: token tree storage and the syntax bridge

Paths: `crates/tt/` (`storage.rs`, `lib.rs`, `iter.rs`, `buffer.rs`) and `crates/syntax-bridge/`
(`lib.rs`, `to_parser_input.rs`). Nothing was built, run or measured in this pass (no builds, no
benchmarks). Every number below is from the layout, and every claim about behaviour is held by a
differential test written beside the code, to run when the lane may build.

## The bar

- **rustc's `TokenStream`**: `Lrc<Vec<TokenTree>>`. A `TokenTree::Token` is about 32 bytes: a
  `Token { kind, span }` with an 8-byte `Span`, plus spacing. A `Delimited` group holds its own
  `Lrc<Vec<..>>`, so it costs one allocation per group. Cloning is cheap (refcount), but iterating
  nested groups chases pointers, and equality walks tokens one by one.
- **The target layout** (from the brief): flat arrays, delimiters as CSR (a subtree is followed by
  its children, and its length skips them), each leaf a tag plus a u32, spans in a side column.

What to judge: bytes per leaf and per delimiter; allocations per tree; the cost to iterate, skip,
compare and build.

## Before

`TopSubtree` was a varint-encoded byte stream: tags, lengths and indices as LEB128, spans as
indices into a part table sorted by frequency, and symbols likewise.

- **Memory.** About 5 to 8 bytes an element.
- **Building.** Collect into temporary `Vec`s, count frequencies in hash maps, sort the tables,
  encode into a reversed buffer, then copy it out.
- **Reading.** Any access decoded from the front, so `last_span` was a walk to the end. Each
  `TokenTreesView` held a slice of decoded state.
- **Equality.** `PartialEq` compared the encodings, which depended on the order of builder calls:
  two trees with the same tokens could compare unequal.

## What changed

### `tt` storage (`storage.rs`)

`TopSubtree` is seven columns. Node 0 is the top subtree, then every token tree in preorder.

| column | per | bytes |
| --- | --- | --- |
| `nodes: [Node]` | token tree | 8 |
| `spans: [PackedSpan]` | leaf, subtree open, subtree close | 8 |
| `wide: [WideSpan]` | span too long or too far to pack (rare) | 12 |
| `parts: [(anchor, ctx)]` | distinct pair | 12 |
| `symbols: [Symbol]` | distinct ident or literal text, nearby | 8 |
| `top_open`, `top_close` | tree | 20 each |

- **`Node { head, data }`.** The tag is the low two bits of `head`.
  - A subtree packs its delimiter kind and its ordinal in `head`; `data` is its length.
  - A punct packs its spacing; `data` is its char.
  - An ident packs its raw flag; `data` is its symbol index.
  - A literal packs its kind, its hashes and its suffix length; `data` is its symbol index.
- **Cost per token tree.** A leaf is 16 bytes and a subtree is 24 (its opening and closing spans
  both sit in `spans`, the closing one after the nodes, at its ordinal).
- **Skipping and access.** Skipping a subtree is one add. Any token tree is one index away.
- **`PackedSpan { start, len_part }`.** `len_part` holds the length in its low 16 bits and the
  part index in its high 16. A span whose length or part index won't fit goes to `wide`.
- **Allocations.** A tree is 4 to 6 boxed slices, whatever its shape. The builder's buffers are
  reused per thread (a TLS `Scratch`, kept up to 2^16 entries), so a build allocates only the tree.
- **Canonical encoding.** The tables list entries in the order the tree first uses them. Part
  lookup is a last-hit cache, then a scan of up to 8 entries, then an Fx map. Symbol lookup is an
  exact scan of a window of the last 16, with no hashing. So the columns are a function of the
  token trees alone, and the derived `PartialEq`/`Hash` are exactly token equality. Equality is a
  slice compare, and build order no longer matters.
- **One pass to build.** The builder records raw nodes, `Span`s and symbols. `finish` packs them,
  with no frequency maps, no sort and no copy out of a temporary buffer.
- **Builder edits.** `restore`, `remove_last_subtree_if_invisible` and `build_skip_top_subtree`
  only truncate or drop raw entries; packing happens after them, so their output is canonical too.
- **O(1) updates.** `set_top_subtree_delimiter_span` and `set_top_subtree_delimiter_kind` change
  the tree in place. `change_every_ast_id` maps `parts` and the top spans in place; it re-encodes
  only when two parts become one.

### Views and iteration (`lib.rs`, `iter.rs`, `buffer.rs`)

- **`TokenTreesView`** is `{ tree, start: u32, end: u32 }`, 16 bytes and `Copy`.
- **`last_span` and `first_span`** are O(1).
- **`TtIter`** steps one node or one subtree per `next`, with no decoding state, and has a
  `size_hint`.
- **`Cursor`** keeps a stack of `(subtree, end)` u32 pairs.
- **`Literal::push_to(&mut String)`** writes the literal's source text with `write_str`; `Display`
  uses the same code.
- **`token_to_literal`** no longer builds a `String` for a suffixed number.

### syntax-bridge

- **One pass, preallocated.** `convert_tokens` takes a length hint and builds with
  `TopSubtreeBuilder::with_capacity`. The hint is the node's element count for a flat node and
  `lexed.len()` for text.
- **No `String` per token.** `SrcToken::to_text` returns `&str` (it was a `SmolStr` per ident,
  literal and lifetime), so the text is interned straight from the source.
- **Puncts.** A punct's char is read at its byte offset, not with `chars().nth`.
- **One range per flat token.** `FlatTokens` reads a token's range once and slices the text from
  it (it was `range` and then `token_text`, which computes the range again).
- **The sink, `TtTreeSink`.**
  - It appends a literal with `Literal::push_to`, not `format_to!`.
  - It builds its `SpanMap` with `with_capacity(parser_input.len())`.
- **`to_parser_input`** checks a last-context cache before its Fx map, so a run of tokens from one
  context costs no hashing.

## After

| | before | after |
| --- | --- | --- |
| bytes per leaf | ~5-8 | 16 |
| bytes per subtree | ~8-12 | 24 |
| allocations per tree | many (temporary vectors, maps, output) | 4-6 boxed slices |
| `last_span` | walk to the end | O(1) |
| skip a subtree | decode its contents | one add |
| equality | encoding compare, order dependent | slice compare, canonical |
| string allocations in the bridge | one `SmolStr` per ident, literal and lifetime | none |

The trade: the tree is about twice the bytes of the varint stream, still half of rustc's per
token, with no per-group allocation. In exchange every access is O(1) with no decoding, and
building is a single pass. The bridge's remaining per-token cost above O(1) is
`FlatTree::range` (up to 8 steps from the last mark); format-tree is asked for an in-order walk in
`v2/requests/format.md`.

## Proof (tests beside the code, not run in this pass)

`crates/tt/src/storage/tests.rs` against `storage/varint.rs`, a test-only port of the old
encoding:

- **`random_trees_match_the_old_encoding`.** 300 seeds of random trees with every leaf, literal
  and delimiter kind, wide spans and many parts. It checks flat tokens, spans, `Display`,
  `last_span`, iteration through `TtIter` and through `Cursor`, and builder replay (with skip-top,
  restore and removing invisible subtrees), each against the old model.
- **`lexed_files_match_the_old_encoding`.** The same checks over the parser's test data and tt's
  own sources.
- **`equality_and_hash_are_those_of_the_token_trees`.** `==` and `Hash` agree with token equality.
- **`spans_too_wide_to_pack_and_more_parts_than_fit`.** 70k parts.
- **`changing_a_tree_in_place_gives_the_tree_of_its_new_token_trees`.**
- **`extending_alone_makes_the_last_punct_alone`.**
- **`literals_are_what_they_were`.** `token_to_literal` against the old code.
- **`literals_display_as_they_did`.** `push_to` and `Display` against the old `write!` code.

`crates/syntax-bridge/src/tests.rs`:

- **`a_flat_tree_converts_as_the_cursor_does`.** Over the corpus and both doc-comment modes,
  the flat path equals the cursor path (`syntax_node_to_token_tree_modified`), on whole files and
  on every fn.

## Unsafe (docs/v2/plan/design/unsafe.md)

Both crates are `#![forbid(unsafe_code)]`. The layout's speed comes from its shape, not from
`unsafe`: indexing is bounds-checked, and a span's part, a symbol or a closing span is one checked
index. Neither crate reads untrusted bytes. `TopSubtree` is built only through its builder from
token trees, so there is no zero-copy view to verify. Decoding stored HIR buffers is in
`hir-def`'s persist code, outside these crates.

## Left

- Spans are still part of `TopSubtree`'s `Eq`/`Hash`, which salsa needs today. Taking them out of
  content hashes needs hir-expand to compare spans separately (format-span's note).
- An in-order token walk in syntax-tree (request filed).
