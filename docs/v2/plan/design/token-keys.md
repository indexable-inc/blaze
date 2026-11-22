---
status: plan
summary: "Plan row X4: an item's keys from its normalised tokens, not its bytes, so re-formatting a file or editing a plain comment enters no task. The token digest's exact encoding, the token-relative place every record keeps instead of a byte range, the line-map cell that layout readers read explicitly, the table of twelve edits, and the census of every place where text bytes feed a key today. The core is written (crates/parser/src/token_key.rs); the switch of the call sites is one later batch."
date: 2026-11-17
---
# Token keys (plan row X4)

The check: tasks entered after formatting a whole crate: 0. Today an item's keys hash its text's bytes
(the census below), so `rustfmt` re-enters every item it touches.

## Decisions

- **D-a. The key of a text is its tokens as the parser sees them.** Per token: its kind, its lexeme's bytes
  (`1_000` and `1000` differ, `r#try` and `try`, a string's contents, a literal's suffix) and whether it is
  joined to the next. Whitespace, newlines and plain comments (`//`, `/* */`) are not tokens. Doc comments
  (`///`, `//!`, `/** */`, `/*! */`) are attributes with meaning (`missing_docs`, rustdoc lints, `#[doc]`
  reflection): they are tokens, as their whole text. The EDITION is in the digest: one text is two programs
  in two editions (keywords such as `async`, `dyn`, `gen`; 2018's `k#x` as three tokens; guarded strings).
- **Joined** is `proc_macro`'s `Spacing::Joint`: a punctuation token that is not a delimiter, written right
  against a next token that is punctuation (not a delimiter) or a lifetime. So `>>`/`> >`, `a --b`/`a - -b`,
  `&&`/`& &`, `&'a`/`& 'a` differ; `f(x)`/`f (x)` and `a+b`/`a + b` do not. All other spacing is layout.
- **D-b. A place in a record is token-relative.** A key that formatting does not move makes a byte range
  stale, so every stored place (a finding, a body's span) is a `TokenPlace`: first and last token index in
  the item, and for a place inside one token a byte range of that token's lexeme. It becomes line:column only
  when printed, from today's text of the item, lexed again (printing is rare; an item is small). Expansions
  already work this way (crates/blaze-tokens: "a position inside an expansion is a `TokenRange`").
- **D-c. A local's rename is not in this piece.** It needs binder-normalised bodies and a way to print the
  names back; a later piece.
- **D-d. Layout is an explicit read, in two cells.** Readers of LINES and COLUMNS (`line!()`, `column!()`,
  `Location::caller()`, a span's line/column, debug line tables) read "this item's line map": each token's
  line (relative to the item's first token) and column. Readers of SPACING (`stringify!`, `assert!`'s
  message, a proc macro's `TokenStream::to_string`/`Span::source_text`) read the finer "this item's
  adjacency": one bit per token, whether trivia separates it from the next; a line break where a space was
  moves the line map and not the adjacency. A reader of an ABSOLUTE line (`line!()`, a line table) also
  reads the item's start line, a place cell of the next piece; `file!()` reads the file's path. Formatting
  then re-enters exactly the tasks that read layout. Wiring readers is the next piece.

## Where it lives

`crates/parser/src/token_key.rs`, beside `LexedStr`: the parser crate is the lowest that lexes by edition
(blaze-lex is rustc_lexer's raw, edition-free tokens, `no_std`, and sees no hasher) and already depends on
blaze-ids. One lex into the thread's scratch columns and one pass; nothing allocated. `token_digest(text,
edition) -> Result<ContentId, TokenKeyError>` (the one error: a text over `u32::MAX` bytes), `line_map_digest`
and `adjacency_digest` likewise, and `TokenPlace::{to_bytes, from_bytes}`. A doc comment's kind is the
lexer's `INNER_DOC_COMMENT`/`OUTER_DOC_COMMENT`; a `COMMENT` is also read by rustc_lexer's doc rule.

## The encoding

`token_digest`, BLAKE3 in domain `"blaze v2 item tokens 1"`: `edition` (1 byte, `Edition as u8`); per token
`1`, kind (`SyntaxKind as u16`, 2 bytes LE), lexeme length (u32 LE), lexeme bytes, joined (1 byte); then `0`.
The `1`/`0` tags make the stream prefix-free. A lex error is no refusal: its token (`ERROR`, or a literal
with its flaw) is hashed like any other, and its message is a function of the text. The kind is a function
of edition and lexeme, but is hashed so the record is explicit; a change of `SyntaxKind`'s numbering is a
new domain.

`line_map_digest`, domain `"blaze v2 item line map 1"`: `edition` (1 byte); per token `1`, line less the
first token's line (u32 LE), column = characters from its line's start (u32 LE; `column!()` less one);
then `0`. A line ends at `\n` only.

`adjacency_digest`, domain `"blaze v2 item adjacency 1"`: `edition` (1 byte); one bit per token, packed
eight to a byte from the low bit, 1 when whitespace or a plain comment follows it (the last token's bit is
0); then the token count (u32 LE).

`TokenPlace { first: u32, last: u32, inside: Option<(u32, u32)> }`. `to_bytes(text, edition)` refuses, by a
typed `PlaceError` naming the counts and never clamping (rule 3): last before first, last past the item's
tokens (`PastEnd { last, tokens }`), `inside` across tokens, past the lexeme or off a character boundary.
`from_bytes(text, edition, range)` takes whole tokens or a stretch inside one (an empty range at or in a
token too) and refuses a range ending in trivia or cutting a token while covering several.

**Expansion tokens do not digest equally** with text tokens, by design: `Tokens::digest`
(crates/blaze-tokens/src/view.rs) hashes the parser word (whose `JOINT` bit is set for any non-identifier
pair, `)(` and `[0` too), `is_joined` for every pair, and each token's hygiene chain. An expansion has no
text, so its spacing is part of what it is (a `stringify!` of it reads that), and hygiene is part of its
meaning. They could share a form later as (token digest, spacing, hygiene); not needed for X4.

## What moves, and what does not

| Edit | Token digest | Line map |
|---|---|---|
| formatting (spaces, breaks, indentation) | no | yes |
| a plain comment added or edited | no | only if it moves a token |
| a doc comment edited | yes | if lines change |
| `>>` split into `> >` | yes | yes |
| `1_000` to `1000` | yes | if columns after it change |
| a trailing comma added (a token) | yes | yes |
| parentheses added (tokens) | yes | yes |
| CRLF to LF between tokens (`\r` is whitespace to the lexer) | no | no (a line ends at `\n`) |
| CRLF to LF inside a string literal or a line doc comment (the lexeme keeps the `\r`; the string's VALUE does not, `parser::unescape` reads `\r\n` as `\n`) | yes | no |
| a blank line added above the item | no | no (relative lines) |
| an attribute reordered | yes | yes |
| tabs to spaces | no | yes (`column!()` counts a tab as one) |
| a `/* */` comment inside an expression | no | if columns change |

**CRLF, checked.** rustc rewrites `\r\n` to `\n` when it loads a file (`normalize_src`). blaze does not
rewrite the text (positions and digests are of the raw bytes) but reads every value by one rule,
crates/parser/src/line_break.rs: a CR LF is one break whose VALUE is LF. Cooked strings
(crates/parser/src/unescape.rs:402), raw strings (unescape.rs:283) and doc comment text (`doc_comment_value`;
a bare CR is the error, cook.rs `doc_comment_errors`) all follow it, so a CRLF file's literals equal
rustc's: no rule-4 bug found on these paths. The lexemes keep the `\r`, so CRLF inside a literal or doc
comment over-moves the token digest (sound, not minimal); hashing such lexemes through `without_break_crs`
would make CRLF move nothing, a later refinement.

## The census: text bytes that feed a key or a cell today

"Switch" is what the batch does: token = `token_digest`; lines = also read `line_map_digest`; place =
store `TokenPlace`.

| # | Where | What it keys | Switch |
|---|---|---|---|
| 1 | blaze-scope/src/names_cells.rs:160 `span_text` | `Item(key)` cell: an item's and an impl header's text | token |
| 2 | blaze-scope/src/names_cells.rs:204 | a module row's head text | token |
| 3 | blaze-scope/src/asked.rs:294 `macro_digest` | a `macro_rules!`/macro 2.0 body | token + lines (its spacing becomes the expansion's `is_joined`) |
| 4 | blaze-scope/src/expansion_records.rs:139 `key_of` | an expansion record's own bytes | stays bytes (a record's content address) |
| 5 | blaze-file-items/src/digest.rs:271 `entry_digests` (runs at :428) | every outline entry's `hash` and `signature_hash` | token (runs of tokens, children by digest) |
| 6 | blaze-file-items/src/digest.rs:205 `key_batch`, `By::Head` | an unnamed item's (impl's) IDENTITY: digest of its head's bytes | token |
| 7 | blaze-file-items/src/split.rs:39 `part_digest` | a top-level part's outline | stays bytes (the outline holds byte ranges; cheap to redo) |
| 8 | blaze-file-items/src/names_digest.rs:119 `file_names_digest` | a file's inner attributes | token |
| 9 | blaze-file-items/src/names_digest.rs:159 `message` | an entry's name, `pub(in path)`, attributes | token |
| 10 | blaze-lower/src/lib.rs:642 `signature_key` | a signature record | token |
| 11 | blaze-lower/src/lib.rs:650 `body_key` | a body record | token; its `Span`s place; line-reading bodies lines |
| 12 | blaze-lower/src/lib.rs:656 `anon_const_key` | an anonymous constant: its byte start and text | token; start as place |
| 13 | blaze-cli/src/signature.rs:262 `tokens` | `unencoded` heads: lexemes only | DONE in this piece: was lexemes only (`1 >> 2` and `1 > > 2` shared it); now `token_digest`, domain `"blaze-cli signature tokens 2"` |
| 14 | blaze-cli/src/signature.rs:226 `signature_source` | a `Signature` task's source | follows 5 |
| 15 | blaze-cli/src/body_types.rs:108 `body_source` | a `BodyCheck` task's source | follows 5 |
| 16 | blaze-cli/src/engine_heads.rs:418 | a head task's source (head + body text digests) | follows 5 and 14 |
| 17 | blaze-cli/src/lints.rs:210 `unit_source` | a lint unit: its bytes and hole offsets | token + lines + a comments cell: the `// SAFETY:` rule (blaze-lint/src/engine.rs:1363) reads PLAIN comments and the line above the item |
| 18 | blaze-cli/src/fix.rs:51 `Id::of` | a finding's shown id: item bytes, byte range | token + place (an id must survive a re-format) |
| 19 | blaze-cli/src/sweep.rs:288 | a file's change digest | stays bytes: the sweep notices any byte change cheaply; a file whose items' token digests are equal then enters no task |
| 20 | blaze-cli/src/work.rs:1916, 1923, 1935 `place: item.range.start` | a FileItems answer row's start in the file (blaze-drive/src/answers.rs:54) | stays bytes (in a record keyed by the file's bytes; never in a source) |
| 21 | blaze-drive/src/record.rs:74 `Finding { start, end }` | a stored finding's item-relative bytes | place |
| 22 | blaze-lint/src/lib.rs:69 `Span` | a lint finding's range | place |
| 23 | blaze-cli/src/units.rs:470 `Record::find` `at: (u32, u32)` | a unit record's finding | place |
| 24 | blaze-body/src/body.rs:100 `Span { lo, hi }` | a statement's item-relative bytes in a body record | place (line tables: lines) |
| 25 | blaze-scope/src/items_record.rs:997 `TextSpan` | where an item's text is in its file | stays bytes (a file's record, checked against its digest) |
| 26 | blaze-ladder/src/front2/body_left.rs:29 `BodyPlace` | an item's first byte in its file, for printing | stays (print only; must never enter a key) |
| 27 | blaze-proc-macro/src/server.rs:586 `Span::line`/`column`/`byte_range` | refused today | lines + the start-line cell when answered |
| 28 | blaze-tokens/src/view.rs:486 `Tokens::digest` | an expansion's tokens | stays (above) |

Names hashed as their text (blaze-file-items/src/chain.rs:48, blaze-cli/src/signature.rs:236, the cell keys
in blaze-scope/src/cell_key.rs) are one token each and stay. Twenty-eight rows; twenty-one switch, seven stay.

## Rule 21: what the token digest must not merge

Read for every switched row: the token digest covers a reader only if it reads no layout and no plain
comment. Readers found that do: the `// SAFETY:` lint (row 17), `stringify!` and `assert!` (blaze-expand's
`stringified` spaces as rustc's `print_tts` does, by each token's spacing), proc macros that print or ask
spans, and `line!`/`column!`. Spacing readers read the adjacency, line and column readers the line map, the
lint a comments cell besides, explicitly, or its row stays on bytes until it does.
