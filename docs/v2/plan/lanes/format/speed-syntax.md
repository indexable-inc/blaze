---
status: plan
summary: speed-syntax: the lexer, parser and reparse of crates/parser and crates/syntax against simdjson, Zig, tree-sitter and rowan; what changed and what is left.
date: 2026-10-29
---
# speed-syntax: lexing, parsing and the item reparse

The speed pass of `crates/parser` and `crates/syntax` (all but format-edit's editing files).
`crates/syntax-tree` was not free (format-tree is running again as format-edit), so its
findings are requests (`v2/requests/format.md`, section speed-syntax). Where the time goes, from
`docs/research/lean-frontend.md`: lexing and parsing to events is 11.5% of a cold start's
innermost frames. Within the parser's samples, `LexedStr::new` is 10%, `event::process` 8%, the
flat tree builder 6.5% and `intersperse_trivia` 3.6%, and the grammar is the rest.

## Lexing (`crates/parser/src/lexed_str.rs`, `lexed_str/scan.rs`)

**Benchmark**: simdjson's stage 1. It classifies 64 bytes at a time into bitmaps with vector
compares, then finds structure with `trailing_zeros`. Zig's tokenizer (one pass over bytes, a
state machine with no Unicode tables) is the scalar bar.

**Before**: every token went through `rustc_lexer`: a `Chars` iterator decoding UTF-8 a
character at a time, `unicode_ident`'s table lookup per identifier character, and a `match` per
character of whitespace. On top of that, every literal heap-allocated an error string
(`String::from("Invalid raw string literal")`, made whether or not it was used), and each
error went through a `Vec<String>` per token.

**After**:
- `scan::Classes` makes bitmaps of the text in one pass, 64 bytes per step, on AVX-512BW,
  AVX2 or NEON, chosen at run time: ASCII identifier bytes, ASCII whitespace, `\n`, and `"`
  or `\`. The whitespace bitmap is then replaced by token starts. They cost 32 bytes per 64
  of text, for the lexing only (format-lexer's section of
  [speed-of-light.md](speed-of-light.md) has the token loop over them).
- `scan::token` lexes from them every token that the bytes alone decide:
  - a whitespace or identifier run is one `trailing_zeros` per 64 bytes;
  - a line comment's end is the next newline bit;
  - punctuation and plain decimal integers take a byte or two of lookahead.
- The rest goes to `rustc_lexer` for that one token: quotes, prefixed and raw strings, block
  comments, `#"`, numbers with a base, exponent, fraction or suffix, and any token next to a
  byte of 0x80 or more. That is dispatch by token class, not a fallback. Both give the same
  `(kind, length)` wherever the fast path answers, and `rustc_lexer` remains the definition for
  the tokens it lexes, so the tokens are exactly rustc's.
- No valid token allocates. Errors are `Cow<'static, str>` pushed straight to the side table,
  and only the formatted messages (an unterminated literal, an unknown prefix) allocate.
- Tests: `every_path_classifies_as_the_scalar_one` covers every vector path against scalar,
  over every length and alignment, on random bytes and real files.
  `the_fast_path_lexes_as_rustc_lexer` checks every char-boundary offset of the parser's
  sources, its 557 test files and 3000 random texts weighted to prefixes, quotes, digits, dots,
  comments, Unicode whitespace and emoji. `fast_and_rustc_lexer_lex_alike` lexes whole texts
  both ways in every edition and compares them, shebangs, frontmatter and a 4 MiB literal
  included.

**Compared now**: the token loop no longer decodes characters for the classes that make up
most of Rust source (whitespace, identifiers, keywords, punctuation, comments). The
classification runs near memory bandwidth, like simdjson's stage 1. Where we are still
behind Zig:
- The per-token work after the scan: the keyword `match` on the identifier's text (generated
  code in `syntax_kind/generated.rs`, which `xtask` writes) and the push.
- The tokens `rustc_lexer` keeps (string and char literals, lifetimes).

Closing the gap would take two things. First, a perfect hash for keywords in the generator,
keyed by length and first two bytes: one compare instead of a chain. Second, owning the
literal grammar too: string and char ends found from quote and backslash bitmaps, as simdjson
finds string bounds. Lifetimes need the Unicode tables only past ASCII.

## Parsing to events and the tree (`event.rs`, `shortcuts.rs`)

**Benchmark**: Zig's parser writes its AST (tag plus data, preorder nodes) as it parses.
tree-sitter's runtime builds its tree with no second pass.

**Before**: `event::process` pushed every `Start` into a scratch list and drained it reversed,
even the 99% with no forward parent. `intersperse_trivia` called its sink through
`&mut dyn FnMut`: one indirect call per token and node, which no inlining could see through,
into the flat tree builder's push.

**After**:
- A `Start` with no forward parent enters at once.
- The sinks are generic (`F: FnMut + ?Sized`, so existing `&mut dyn` callers still compile):
  the builder's push inlines into the walk.
- `Event` has a size assertion (8 bytes).

The parse stays two passes, events then output, because `precede` and `forward_parent` need
the events. Zig's single pass would mean the grammar writing nodes in postorder and reversing
once, a rewrite of `grammar/`. That rewrite would remove `event::process` (8% of the parser's
samples).

## Outline and bodies (`parsing.rs`, `lib.rs`)

**Before**:
- `Parse::errors_with_bodies` and `errors()` parsed every body a second time
  (`body_parser_errors`) to find its parser errors.
- Each body's part copied the body text into its own `Arc<str>`.

**After**: `expand_lazy` returns its parser errors to syntax-tree (format-tree's new `Expand`),
and `parsing::body_errors` reads them from `FlatTree::part_errors`. Each body is parsed once,
whoever asks for its tree or its errors. The illegal-float error keeps its quirky position (a
token number in the file), now computed only when that error occurs. The copy of the body text
remains until syntax-tree's request 2 lands (a part built over the file's own text).

## Validation (`validation.rs`)

`validate_node` ran on every node with `match_ast!`: 15 casts per node, each a cursor clone
and a kind read. Now the kind is read once and compared with each type's kinds, and the node
is cast (one clone) only for the type that matches.

## The item reparse (`parsing/reparsing.rs`, `lib.rs`)

**Benchmark**: tree-sitter's incremental reparse, which reuses every subtree outside the edit
and reparses the smallest changed region.

**Before**:
- The new text was built twice, once into a `String` and again by `Arc::from`: two
  copies of the file per edit. `full_reparse` did the same.
- A token edit rebuilt the token's parent from the arrays to splice it. For top-level
  whitespace or a comment that parent is the whole file, and the rebuild dropped every
  parsed body.
- `lexes_alone` collected each lexing into a `Vec` to compare them.

**After**:
- `raw::joined` writes the new text straight into one `Arc<str>` (one copy, one
  allocation).
- `Edit::token` calls `FlatTree::replace_token`. That writes one word, the big-length table and
  the starts after it, and keeps every lazy node and parsed part.
- `lexes_alone` compares the token iterators directly.

**Compared now**: like tree-sitter, an edit inside a token, a `fn` body, a block or a top-level
item reparses just that and splices it in. We are behind tree-sitter in two ways:
- The splice copies the tree's arrays, `O(file)` words of `memcpy`, where tree-sitter shares
  unchanged subtrees by pointer. Sharing would mean a rope of array chunks in syntax-tree, a
  format change. The `memcpy` runs at memory speed (6.5 bytes an element), so a 10,000-line
  file is a few hundred microseconds.
- The candidate's text is copied once (`Arc::from(text)`) until syntax-tree's request 2 lands.

## Walking and the flat tree (`crates/syntax-tree`, not free)

**Benchmark**: rowan is the baseline it replaces (about 40 bytes and an allocation per node).
Carbon's and Zig's trees are the bar (8 to 12 bytes a node, preorder, parents and offsets by
arithmetic). format-tree's packing already meets data.md's budgets (a token 4 bytes, a node 8,
about 6.5 bytes an element), with a test asserting them.

Two findings are filed as requests, because syntax-tree belongs to format-tree:
- A sourced builder compares every token's bytes with the source, a second pass over every
  byte parsed. An address compare is as strong.
- A part is built over a copy of its text and then moved. `Expand` should get the file's
  `Arc<str>` and range, with a `sourced_range` builder.

## Unsafe (docs/v2/plan/design/unsafe.md)

Both crates are `#![deny(unsafe_code)]`, with `#[allow(unsafe_code)]` only on the places
below. `forbid` is not possible, because each crate keeps one unsafe module. Each unsafe block
does one operation and has a `// SAFETY:` comment naming its invariant and where it is upheld.
Each `unsafe fn` has a `# Safety` section. Each place has a safe reference version that a test
checks against it.

| Place | What is unsafe | The safe version and its cost | Test against it |
| --- | --- | --- | --- |
| `parser` `lexed_str/scan.rs` | vector loads, `target_feature` calls | `blocks_scalar`, a table lookup per byte | `every_path_classifies_as_the_scalar_one` |
| `parser` `syntax_kind.rs` `From<u16>` | `transmute` after the bounds assert (it was already there) | a match of every kind | the assert; values come from the generated table |
| `syntax` `raw.rs` `joined` | copy into an uninitialised `Arc<[u8]>`, `assume_init`, cast to `str` | `concat()` and then `Arc::from`: a second copy of the file and allocation per edit | `joined_is_the_concatenation` |

No input to these places is untrusted: they read in-memory `&str` and `&[u8]` slices whose
bounds are checked by slicing before each load or copy. There are no pointer offsets: subslices
replace `ptr.add`. Nothing in `crates/syntax-tree` was changed (not free). There were no
safety-lane requests for these paths as of 10-29.

## Not changed, and why

- `parser`'s grammar (70% of the parser's samples) is rust-analyzer's recursive descent. Its
  lookahead is already cheap: `TokenSet` is a three-word bitset and `Input` is columns.
- `lazy_block_len`'s attribute stack allocates only for a body that has attributes.
- `Input` keeps an edition per token (1 byte), which macro inputs need.
