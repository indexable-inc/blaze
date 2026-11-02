---
status: plan
summary: blaze v2 keeps one flat token format from the lexer to macros and the parser - token-tree columns with spans in a side table - which the parser's input is read from in one pass with no span or leaf decoded, and which the proc-macro wire sends as its columns (frontend.md item 4).
date: 2026-10-29
---
# blaze v2: one flat token format (frontend-tokens)

Design notes of lane `frontend-tokens` for item 4 of
[the front end's design](../../design/frontend.md), as written on 2026-10-29. Nothing was built
or run (the lane may not build): every claim below is what the code is written to do, and each
test named is what proves it once it runs.

## The format

`tt::TopSubtree` is the one token format. Its columns:

- **Nodes.** One 8-byte node per token tree, in preorder, the top subtree first. A tag and small
  fields go in one word; a char, a symbol index or a subtree's length go in the other. Skipping
  a subtree is one add.
- **Spans, a side column.** One packed 8-byte span per node, then one per subtree for its
  closing span. A packed span is a range plus an index into the table of (anchor, context)
  pairs the tree uses; spans too large to pack go to a `wide` table.
- **Symbols.** The interned text of idents and literals.

The columns are a function of the token trees alone, so equal trees are equal and hash equal
whichever builder calls made them. That was there before this lane. What the lane adds is that
nothing between the lexer, the matcher, the parser and the proc-macro wire converts tokens into
another per-token structure:

| Step | Before | Now |
| --- | --- | --- |
| Lexed text to tokens (`parse_to_token_tree*`) | `LexedStr::text(i)`, which finds the token's start from a checkpoint (up to 16 steps) | `LexedTokens` keeps a running offset: one load for each token |
| Tokens to parser input (`to_parser_input`, `to_matcher_input`) | a `Cursor` decoding each leaf with its span, the identifier's kind found from its text, the edition from an `FxHashMap` keyed by context | `TokenTreesView::for_each_token`, a walk over the node column that unpacks no span and decodes no leaf, keywords from a table by symbol, editions from an array of the last 8 contexts |
| Parser input | five columns (`Vec<SyntaxKind>` twice, `Vec<Edition>`, two bitsets), allocated per call | one `u32` word a token (kind, contextual kind, edition, joint, fragment start), reused per thread through the `_into` functions |
| Macro matcher (`expect_fragment`) | a new input for each fragment window | one input for each thread, refilled |
| Proc-macro wire | JSON `FlatTree`, or the wasm `Writer` with one `FxHashMap` probe a span | `TopSubtree::write_wire` and `read_wire`, the columns as they are (requested of usercode, see below) |

### Why the parser takes an input built in one pass, not the columns themselves

The parser indexes tokens by position (`nth(n)`, lookahead) and expects lexer tokens. The token
trees differ from that in three ways:

- a lifetime is `'` plus an ident;
- a subtree is one node, standing for two tokens (its open and close);
- groups without delimiters are no tokens at all.

So the input can't be a view of the node column without changing the grammar crate's token
model. The input is instead written in one pass over the columns, at 4 bytes a token, with no
strings read but a literal's float check, no hashing for an ident whose symbol is declared or
whose length is no keyword's, and no allocation once the thread's buffer is warm. A real no-op
view would need the parser to read nodes directly: one lookahead decode of the tag, with `'`
handled as a lifetime and subtrees as two tokens. That is a change to the grammar's input trait
that `parser/` alone can't make while `syntax/` (frontend-deps) still builds trees from lexer
tokens. It is the next step once both sides read the same trait.

### Keywords by symbol

`Keywords` maps every strict and contextual keyword text, from
`SyntaxKind::keyword_texts`, to its kinds in every edition.

- **Building.** It is built once from `SyntaxKind::from_keyword` and
  `from_contextual_keyword`, so it gives the same answer as they do by construction.
- **A declared symbol.** If the ident's symbol is a declared one, the lookup is
  `declared_index`, then a byte table, then the kinds.
- **Any other symbol.** A length mask comes first, so an ident that is no keyword's length is
  done without a probe; only one of a keyword's length probes an `FxHashMap` by symbol id.

### Spans stay a side table

The parser never sees spans. `TtTreeSink` reads the spans of the tokens the parser consumed
from the same view to build the `SpanMap`, as before. A macro call's input is written from a
view of the call (`syntax_bridge::call_input`), one pass over the call's slice of the file's
flat tokens with spans from the side table in order; an attribute or derive input, which strips
cfgs and applies fixups, from `call_input_modified`, the same walk with removals and appends
keyed by `NodeId`. Neither makes a syntax handle. hir-expand's move onto both is requested of
db-expand (`v2/requests/db-expand.md`, the two "from a view" sections); the handle versions stay
for `expand_speculative` (IDE, edited green trees) and as the tests' reference. An expansion's
output is parsed straight from its columns.

### The proc-macro wire

`tt::TopSubtree::write_wire` writes the columns as little-endian words: the header, nodes,
packed spans, wide spans, the parts as raw `(file, AST id, context)` ids, the symbols' end
offsets, the two top spans, then the symbols' UTF-8 (layout in `tt/src/storage/wire.rs`).

`read_wire` checks the bytes before trusting them:

- every rule the tree's methods rely on: tags, subtrees nesting inside their parents, ordinals in
  order, indices in range, chars, UTF-8, ranges under 4 GiB, a literal's suffix being a tail of
  its text;
- that the columns are exactly the builder's (it replays the encoder), so a tree read compares
  and hashes as the same tree built;
- each span part, which goes to a callback the reader supplies and is refused unless it is one
  that side handed out.

It allocates at most a constant times its input, after checking each column's length against
the bytes left. The crates that send trees (`proc-macro-api`, `proc-macro-srv`,
`blaze-macro-wasm`) are lane usercode's; the change to use the wire there is requested in
`v2/requests/usercode.md` (`## frontend-tokens: send token trees over the proc-macro wire as
their columns`).

## The benchmark it is held to

- **rustc.** Its parser reads `TokenStream`s through a `TokenCursor`, so there is no conversion
  step at all. Each token is a 24-byte `Token` (kind plus an 8-byte `Span`) inside an
  `Arc<Vec<TokenTree>>`, and spans are interned. The proc-macro bridge crosses the boundary one
  token tree at a time through RPC handles.
- **Parsing the token trees.** blaze's parser input is 4 bytes a token, written in one pass that
  touches the 8-byte node column (16 bytes a leaf with its span, which the pass doesn't read).
  That is fewer bytes a token than rustc's cursor reads, but there is still one pass rustc
  doesn't have. The goal is to match rustc's zero conversion with the parser-reads-nodes trait
  above.
- **The wire.** A proc-macro call's tokens cross as bulk copies of the columns each way, against
  rustc's per-tree RPC and the old JSON.
- **The structural index.** The preorder layout, with a subtree's length in its node, is the
  same as simdjson's structural index: skipping a group is one add, and no pointer is chased.

Nothing was measured: the lane may not run benchmarks. `crates/mbe/src/benchmark.rs` (the
real-world macros) and the lexed corpus in `tt`'s and `syntax-bridge`'s tests are what to
time against the old path, which `syntax_bridge::reference` keeps.

## What proves what

The old conversion is kept for tests only as `syntax_bridge::reference` (the crate's tests,
and other crates' tests through the `reference` feature that mbe's dev-dependency turns on).

| Test | Proves |
| --- | --- |
| `tt` `storage/tests.rs` `check`, over 300 random trees and the lexed corpus | `for_each_token` gives exactly the token sequence of the token trees (opens, leaves, closes, with each one's context), for a whole tree and for the inside of its top subtree |
| `tt` `check_wire`, called from `check` | `read_wire(write_wire(t)) == t` for every tree. For trees of up to 300 token trees, every proper prefix of the bytes is refused, and every one-bit flip is either refused or reads as a tree that writes back exactly those bytes (no corrupt input reads as a non-canonical tree or panics) |
| `parser` `input.rs` `words_read_back_their_fields` | the packed word keeps kind, contextual kind, edition, joint and fragment start |
| `syntax-bridge` `to_parser_input.rs` `keywords_answer_as_the_text_functions` | the keyword table answers as `from_keyword` and `from_contextual_keyword` for every keyword and some non-keywords, in every edition |
| `syntax-bridge` `editions_are_asked_once_a_context` | the caller's edition function is asked once per context |
| `syntax-bridge` `tests.rs` `parser_input_is_the_old_conversions` | over the corpus (every file and every `fn`, both doc-comment modes), and over the same trees regrouped with random groups without delimiters (some empty) and runs of four editions' contexts: the parser and matcher input of each view equals the reference's token by token, and parsing as six kinds of expansion gives the same tree, errors and span map (or both panic alike) |
| `syntax-bridge` `lexing_then_parsing_gives_the_files_tree` | lexing a file to token trees gives what converting its syntax tree gives (so the running-offset lexer path changes no token), and parsing those token trees gives the file's tree but its trivia |
| `mbe` `tests.rs` `check_` (every mbe fixture) | each expansion parses to the same tree and span map through the new and the reference input |
| `mbe` `differential.rs` `assert_same` (corpus, benchmark macros, 40 seeded random `macro_rules!` with 5 inputs each, 400 with slow tests) | every input and every expansion in every call style has the reference's parser and matcher input and parses to the same tree, errors and span maps as five kinds of expansion; `expect_fragment` with the reused input takes the same fragments as the model |
| `tt` `walking_and_the_wire_allocate_the_same_for_any_size_of_tree` | on a small tree and one of 40,000 token trees with a wide span: walking the tokens and the token trees allocates nothing, writing into a buffer with room allocates nothing, building with a warm builder allocates once (the tree's block), and reading allocates at most 3 times (the block and the checks' buffers) |
| `tt` `a_tree_read_from_the_wire_has_the_built_block` (proptest) | a tree written to the wire and read back has the block words of the tree built, so equality and hashing hold across the wire |
| `syntax-bridge` `a_reused_input_converts_without_allocating` | converting a tree into a parser's or matcher's input that was used before allocates nothing |
| `mbe` `a_fragment_allocates_the_same_however_much_follows_it` | `expect_fragment` allocates the same with 100 or 10,000 token trees after the fragment |

## Memory (docs/v2/plan/design/memory.md)

Where the token-tree paths allocate, by lifetime:

| What | Lifetime | Now | Plan |
| --- | --- | --- | --- |
| Builder buffers, the walk's open-subtree stack, mbe's matcher input | per call | kept per thread at their high-water mark (mbe's is freed above 64K tokens) | as is; the budget tests hold warm calls to 0 |
| A built `TopSubtree` (expansions, macro arguments, proc-macro answers) | its salsa memo's, which has no lifetime group | one `Box<[u32]>` block from the global allocator: a header of column offsets and lengths, then the columns, every reference an offset; the encoder's columns and tables are kept per thread, so a warm `build` allocates once (`walking_and_the_wire_allocate_the_same_for_any_size_of_tree`) | per mem's answer (`v2/requests/mem.md`): stays on the global allocator until memos persist into the image, then the same words become a checked view of the mapped pack behind the same accessors |
| A tree read from the proc-macro wire | a request | the same one block, written after every column is checked, plus the checks' buffers (at most 3 in all) | as is |
| Errors | rare | global allocator | as is |

## Item 4: the last two conversions (from 2026-10-29 19:54)

Two conversions are still on the build's path. The first is a macro call's input:
`syntax_node_to_token_tree` reads the call's flat tokens, but builds every token's node, span
and kind. The second turns a token tree into `parser::Input` for every parse of an expansion
and every fragment a matcher tries. The design removes both by giving tokens their parser
word once, when they are made, and keeping it in the tree.

- **The parser reads words where they are.** `parser::Tokens<'t>` is a borrowed slice of the
  parser's `u32` words (kind, contextual kind, edition, joint, fragment start), plus whether
  fragment starts count. `Parser` reads it, and an `Input` is one way to get it
  (`From<&Input>`). The entry points and the outline's `lazy_block_len` take
  `impl Into<Tokens<'t>>`, so their callers do not change, and the grammar does not change at
  all.
- **A token tree keeps its words.** `TopSubtree` gets a `words` column in parser order:
  - an open and a close for each visible delimiter, a word for each leaf, and one
    `LIFETIME_IDENT` for a `'` and the ident after it;
  - nothing for an invisible delimiter, whose group's first token carries the fragment start.

  A `word_at` column (one per node, and one per subtree close) gives where each token tree's
  words begin. Any view of whole token trees is then a slice of `words`, so the parser's input
  of an expansion is `Tokens::new(&words[..], false)` and a matcher's is the remaining view's
  slice with fragments. Neither is converted.
- **Where words come from.**
  - A call's input is written from a view of the call (`syntax_bridge::call_input`): one pass
    over the flat tree's tokens, with spans from the side table in order. Its words are made at
    the first fragment a matcher tries, so a proc macro's input, which only goes over the wire,
    never pays for them. It cannot be a literal view of the lexed tokens: it is memoized and
    sent over the wire, and its tokens differ from the lexer's (split puncts, lifetimes as `'`
    and an ident, doc comments desugared, fragment groups from expansion maps).
  - An expansion's words are made at its one parse (`parse_macro_expansion` is memoized), in
    one pass over its columns that writes the 4-byte word each token needs anyway. Transcription
    does not copy its fragments' words: it re-marks contexts (`apply_mark`), so copied words
    would carry the input's editions, and the kept-editions check would have to remake them.
    Copying would save only the keyword lookup by symbol in a pass that must write every word.
  - Trees built by hand (built-in macros, fixups) and trees read from the proc-macro wire get
    theirs the same way, at their first parse or fragment.
- **Faithful to today, held by the differential tests against `syntax_bridge::reference`.**
  The words of every view equal the reference's `to_parser_input` / `to_matcher_input`,
  including fragment starts: a fragment start after an empty invisible group counts only when
  the group is in the view.
- **Padding.** Since 2026-10-29 20:05 (requests-parser, format-grammar's speed request) a
  `Tokens`'s words end in 4 `EOF` words, so the parser's lookahead reads need no bounds check.
  A tree keeps its words padded, as `Input::into_words` gives them. `TokenTreesView::words`
  gives the words from the view's first word to the end of the tree, plus the view's length.
  A view whose words are followed only by the pad (an expansion, or the last fragment a
  matcher tries in a group) is read in place. One that ends earlier is copied into a reused
  `Input` by `Input::extend_from_words`, which checks each word's kinds. That copies 4 bytes a
  token and recomputes no kind or edition.
- **The kept words record their editions.** A word holds its token's edition, and callers
  give the edition function (`ctx.edition(db)` on the build's path, a constant in tests and
  the IDE). So `TreeWords` keeps each context's answer. `syntax_bridge::view_tokens` asks the
  caller's function again for those few contexts. If any answer differs, it makes that view's
  words afresh, by the same conversion, and the kept ones stay. The top-delimiter setters,
  which change a tree in place, drop its words.

Order of work, as done: `parser::Tokens` (additive), then the `Parser` reading it (with
frontend-deps' `outline.rs` signature), then the `tt` columns with their builder and wire, then
the call input from views (`call_input`, `call_input_modified`), then mbe's `expect_fragment`
and `token_tree_to_syntax_node` on kept words. Each step is checked against the reference.
Then a tree's columns become one `Box<[u32]>` block (mem's answer in `v2/requests/mem.md`;
child `frontend-tokens-block`).

| Test | Proves |
| --- | --- |
| `syntax-bridge` `a_views_call_input_is_the_cursors_token_tree` | `call_input` of a view (the file, each `fn`, each token tree; full and outline parses; both doc-comment modes) is the cursor's token tree |
| `syntax-bridge` `a_views_modified_input_is_the_cursors` | `call_input_modified` with random removal and append sets and an `on_event` that skips, drops and inserts gives the cursor's `_modified` tree, over the same views |

