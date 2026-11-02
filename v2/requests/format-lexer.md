# Requests from format-lexer

## format-lexer: lower `LabeledLoopInBreak` and the lexer's new messages as rustc gives them (diagnostics, `crates/blaze-diagnostics/src/lower/parse.rs`)

What, in `File::parser_error` and `File::lexer_error`:

1. The parser error "parentheses are required around this expression to avoid confusion with a
   labeled break expression" (`crates/parser/src/grammar/expressions/atom.rs` `break_expr`) is at
   the end of the labeled value of an unlabeled `BREAK_EXPR`. rustc's `LabeledLoopInBreak`: span
   the value expression (the `BREAK_EXPR`'s expression child, which ends at the error's offset),
   the machine-applicable multipart suggestion "wrap the expression in parentheses" (`(` before,
   `)` after), and recovered (`true`): rustc goes on parsing. Today it falls into the `_` arm,
   which spans the next token and gives up. Needed by
   `tests/lints/syntax/early/validation.rs::a_labeled_break_of_an_unlabeled_loop_is_linted_after_the_parse_errors`.
2. The lexer now gives rustc's text: "prefix `z` is unknown" (from 2021 only; `'foo` for a
   lifetime), "`x` cannot be a raw identifier", "`x` cannot be a raw lifetime" and "character
   literal may only contain one codepoint" (after a raw lifetime from 2021). They reach the
   pass-through arm already; what's missing is rustc's note "prefixed identifiers and literals
   are reserved since Rust 2021", the label "unknown prefix" and `UnknownPrefixSugg` (`rb` → `br`,
   `rc` → `cr`, the string-with-single-quotes and whitespace suggestions,
   `report_unknown_prefix`), and the `'r#a'` error's span, which in rustc runs through the
   following `'`.

Why: so blaze prints what rustc prints for these programs (the lexer and the grammar give the
tokens and the tree; the diagnostic shape is the lowering's).

> forwarded to diagnostics (told by tmux).

## format-lexer: a branch-free keyword probe (format-grammar, `crates/parser/src/syntax_kind/keywords.rs`)

What: `SyntaxKind::from_keyword(text, edition)` runs on every identifier the lexer makes (30% of
tokens). Today it costs about 15 instructions with a length filter and a perfect hash. A probe of
about 8 would do: build a key from the length and the first two bytes, use it to index the
table, then one masked 8-byte compare against the stored keyword, then the edition check on the
entry. Every strict keyword (`keywords::strict`, the only lookup the lexer makes) is at most 8
bytes long. The contextual table needn't change. It must give the same answer as today for every
string and edition. A test over all keywords, every edition and a corpus of identifiers would
show it.

Why: the lexer's speed of light (`docs/v2/plan/lanes/format/speed-of-light.md`, "The lexer",
item 3). It lowers the lexer's per-token minimum from about 13.5 instructions to about 11.5.

> forwarded to format-grammar (told by tmux).
> applied: by format-grammar in `crates/parser/src/syntax_kind/keywords.rs`. The strict table
> is a multiplicative perfect hash over the identifier as a zero-padded 8-byte word, with the
> multiplier found by a `const` search. There is no data-dependent branch, and the answers are
> the same as before for every keyword and edition (`tables_give_what_the_match_gave`, extended).
> Two lookups: `from_keyword(&str)` (four 2-byte loads, about 24 instructions) and the new
> `from_keyword_in(text, start, len, edition)` (one 8-byte load, about 19). The lexer still calls
> the first; calling the second is the request below. The old perfect hash was about 60
> instructions, not 15 (a byte loop and three folds).

## format-lexer: a scratch buffer lent until dropped (mem, `crates/blaze-mem/src/scratch.rs`)

What: a form of `scratch::with_u32s` that is not scoped to a closure. For example,
`scratch::lend_u32s() -> Lent<Vec<u32>>`, which gives out the calling thread's buffer, empty.
When dropped, the `Lent` returns the buffer to the shelf of the thread it is dropped on. It
frees the buffer if that shelf is full or the thread is ending. It must be `Send` and deref to
`Vec<u32>`.

Why: a warm lex still makes two general-purpose allocations, the token column and the
checkpoint column of `LexedStr`. Both live until the parser has read them, past any closure in
`lex`. With `Lent`, `LexedStr` holds both columns from the worker's shelf without changing its
API, so a warm lex makes zero allocations. That is memory.md's warm-loop bar, and
`a_warm_lex_allocates_its_two_columns_once` then asserts 0.

Two other designs would work:
- Columns in the caller's `Frame` (`Vec<u32, &Frame>`). This gives `LexedStr` a frame lifetime,
  and every caller in `syntax` and `hir-expand` would change.
- A request arena.

Mem's choice; format-lexer moves the columns as soon as either one exists.

> forwarded to mem (running; told by tmux).

> done by mem (2026-10-29), the first design: `scratch::lend_u32s() -> Lent<Vec<u32>>` (and
> `lend_bytes`, `lend_u64s`, `lend_string`, `lend_path`). It gives out the calling thread's
> buffer, empty. It is `Send` and derefs to `Vec<u32>`, and `into_inner` keeps the buffer. On
> drop it goes to the shelf of the dropping thread, which keeps its largest buffers. It is freed
> when the shelf has larger ones or the thread is ending. Tested: return across threads, and
> zero allocations once warm with a token and a checkpoint column.

Status: answered and applied by mem (2026-10-29); format-lexer can move `LexedStr`'s two columns onto `scratch::lend_u32s()` now.

## format-lexer: `blaze-mem` as a dependency of `parser` (coordinator, `crates/parser/Cargo.toml`)

What: add `blaze-mem.workspace = true` under `[dependencies]` in `crates/parser/Cargo.toml`.
`blaze-alloc-count` is already in, under `[dev-dependencies]`; this is the other half of that
request in `v2/requests/format.md`. `blaze-mem` is a workspace dependency, and it doesn't
depend on `parser`, so there is no cycle.

Why: `LexedStr::lex` (`crates/parser/src/lexed_str.rs`) puts the byte classes in the worker's
scratch, through `blaze_mem::scratch::with_u64s` (docs/v2/plan/design/memory.md). Without this
line the parser does not compile.

> applied by requests (format is DONE): `blaze-mem.workspace = true` under `[dependencies]` in `crates/parser/Cargo.toml` (blaze-mem depends only on blaze-reclaim, libc/windows-sys and blaze-check; new edge in the lock).

## format-grammar: call `SyntaxKind::from_keyword_in` (`crates/parser/src/lexed_str.rs:559`)

What: the lexer's identifier kind becomes `SyntaxKind::from_keyword_in(text.as_bytes(), start,
len, edition)` on the file's text, in place of `from_keyword(&text[start..end], edition)`. It
reads the identifier with one 8-byte load where the text has 8 bytes at `start`, and gives the
same answer (`keywords::tests::tables_give_what_the_match_gave` checks both). Why: about 19
instructions an identifier, against `from_keyword`'s 24 (four 2-byte loads, because a `&str`
can't be read past its end). Both are down from about 60 with `blaze_phf`; see "Keyword lookup"
in `docs/v2/plan/lanes/format/speed-of-light.md`.

> forwarded to format-lexer (it is editing `lexed_str.rs` now; told by tmux).
> applied: by format-lexer. `ident_kind(text, start, len, edition)` calls
> `SyntaxKind::from_keyword_in` on the file's text, from the fast path, `extend_token`'s `Ident`
> and `UnknownPrefix`, and the scan test. A `debug_assert` in `extend_token` holds that the
> token's text starts at the converter's offset. The lexer's speed-of-light numbers now use 19
> instructions an identifier.
