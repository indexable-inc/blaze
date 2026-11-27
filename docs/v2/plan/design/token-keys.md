---
status: plan
summary: "Plan row X4: an item's keys from its normalised tokens, not its bytes, so re-formatting a file or editing a plain comment enters no task. Built for item keys: blaze-file-items feeds every outline entry's `hash` and `signature_hash` by its tokens (`parser::token_key::joined`), except an item that calls a macro or carries a non-inert attribute, which is still fed by its bytes; stored places are `TokenPlace`. The line-map and adjacency cells are built, not wired. Here: the token digest's encoding, the token-relative place, the two layout cells, the table of twelve edits, and what the digest must not merge."
date: 2026-11-17
---
# Token keys (plan row X4)

The check: tasks entered after formatting a whole crate: 0.

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
  then re-enters exactly the tasks that read layout. The readers are not wired: until they are, an item
whose own tokens call a macro or carry a non-inert attribute is fed by its bytes (blaze-file-items
digest.rs `ByTokens`, `ItemFlags::MACRO_CALL`).

## Where it lives

`crates/parser/src/token_key.rs`, beside `LexedStr`: the parser crate is the lowest that lexes by edition
(blaze-lex is rustc_lexer's raw, edition-free tokens, `no_std`, and sees no hasher) and already depends on
blaze-ids. One lex into the thread's scratch columns and one pass; nothing allocated. `token_digest(text,
edition) -> Result<ContentId, TokenKeyError>` (the one error: a text over `u32::MAX` bytes), `line_map_digest`
and `adjacency_digest` likewise (built, not wired: no caller), and `TokenPlace::{to_bytes, from_bytes}`.
The item keys use the same rule without the whole-text digest: blaze-file-items `Item::hash` and
`signature_hash` are a Merkle step over each run of tokens between child entries, a token joined as
`token_key::joined` says (blaze-file-items lib.rs); records keep `TokenPlace` (blaze-cli engine_lines.rs). A doc comment's kind is the
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

## What stays bytes

A file's change digest (blaze-cli sweep.rs: any byte change is noticed cheaply; a file whose items'
token digests are equal then enters no task), a record's content address, an expansion's tokens
(blaze-tokens `Tokens::digest`), and where an item's text is in its file (print only, never in a key).

## Rule 21: what the token digest must not merge

Read for every switched row: the token digest covers a reader only if it reads no layout and no plain
comment. Readers found that do: the `// SAFETY:` lint (row 17), `stringify!` and `assert!` (blaze-expand's
`stringified` spaces as rustc's `print_tts` does, by each token's spacing), proc macros that print or ask
spans, and `line!`/`column!`. Spacing readers read the adjacency, line and column readers the line map, the
lint a comments cell besides, explicitly, or its row stays on bytes until it does.
