---
status: plan
summary: syntax-tree, parser and syntax under unsafe.md: which unsafe stays (one block per tree, raw counted cursor roots, the lexer's vector loads, one checked transmute, one Arc<str> built in place) and what each buys, what became safe, the reference tests and the fuzz target for source text.
date: 2026-10-30
---
# speed-unsafe-front-syntax: the syntax crates under unsafe.md

The rules are [unsafe.md](../../design/unsafe.md). For each unsafe use below: what the safe
version would cost, and whether it stayed. Every crate root says `#![deny(unsafe_code)]` with
`#[allow(unsafe_code)]` on the `mod` line of each unsafe module and nowhere else; every test and
example root without unsafe says `#![forbid(unsafe_code)]`. All three crates take the workspace
lints (`unsafe_op_in_unsafe_fn`, `undocumented_unsafe_blocks`, `multiple_unsafe_ops_per_block`).

## syntax-tree

| | before | after |
|---|---|---|
| `flat/block.rs` | 12 (3 blocks of two ops) | 14, one op each |
| `cursor.rs` | 8 (7 blocks, 1 `unsafe fn`) | 4 blocks |

- **`flat/block.rs`, kept.** A tree's twelve columns are one allocation. The safe version, a
  `Box<[T]>` per column, is twelve allocator calls and frees per tree instead of one, on every
  parse, every lazy body's part (most trees are small parts, where the calls weigh most) and
  every edit, plus twelve headers' worth of memory. Reads cost the same (a slice, bounds-checked
  per index), so nothing is unchecked on the read side. `put_joined` and `fill` now take the
  pointer and write it in two blocks; `Drop`'s `Layout::from_size_align_unchecked` is the checked
  constructor (one compare per tree dropped).
- **`cursor.rs`, kept, smaller.** The cursor is rowan's: a boxed root with a `Cell` count per
  file, part roots owned by the file root. Safe versions measured against it: `Rc` roots with the
  file's parts held as `Weak` free a part's root when its last cursor goes and allocate it again
  on the next entry (an allocation per entry, against one per part); naming a part by its lazy
  node's index finds its tree through the file's tree on every step (several dependent loads,
  against one). Neither is the same cost, so the raw roots stay. Made safe at no cost: the two
  `NonNull::new_unchecked(Box::into_raw(..))` (now `NonNull::from(Box::leak(..))`), and the
  `unsafe fn flat_unbound` (now `Ref::inner_tree(&self)`, its lifetime the cursor's borrow, so
  no caller can pass a root it does not hold).
- **Threads.** Cursors are `!Send` (the count is a `Cell`; `NonNull` fields), so they are never
  shared across threads and need no model-checked test. What is shared, `FlatTree` (parts in
  `OnceLock`, hashes in atomic `Slot`s), is std's synchronization in safe code.
- **Untrusted bytes.** None: a flat tree is only ever built from text (`FlatTreeBuilder`) and
  only `Serialize`d (`serde_impls.rs`); nothing decodes one from store or image bytes.

Tests: `flat::block::tests::a_trees_columns_fill_its_block_and_pack_again_to_the_same_bytes`
(64 seeded trees reaching every side table: each tree's columns lie in its block, overlap
nowhere and fill it to the byte; `Clone`'s repacked copy equals the `Vec`-per-column copy of the
original), beside the existing column tests; `cursor::tests::a_cursor_into_a_part_keeps_its_file_and_the_last_frees_both`
(the counted roots: a part's cursor outlives every other cursor and keeps both roots, the last
frees both, shown by the trees' `Arc` counts). The cursors' behavior over outlines is already
compared with a model over seeded trees in `flat.rs`'s tests.

## parser

| | before | after |
|---|---|---|
| `lexed_str/scan.rs` | 13 | 13 |
| `syntax_kind.rs` / `syntax_kind/raw.rs` | 1 (on an `impl`) | 1 (its own module) |
| `input.rs` `kind_of` | 1 (on a `fn`) | 0 |

- **`lexed_str/scan.rs`, kept as it was.** The byte classes, 64 bytes a block, are made with
  AVX-512BW, AVX2 or NEON chosen at run time (`is_*_feature_detected!`), the scalar table lookup
  per byte otherwise; the vector loads and the `target_feature` calls are the unsafe ops, one per
  block, each with its detection named. Bit-identical to scalar: `every_path_classifies_as_the_scalar_one`
  (every path in `paths()`, 300 random lengths and 50 real files at 4 alignments), plus
  `the_next_start_is_where_the_run_ends` and the lexers' differentials against `rustc_lexer`
  (`the_fast_path_lexes_as_rustc_lexer`, `fast_and_rustc_lexer_lex_alike`). This is simdjson's
  stage 1 shape; the safe version is the scalar path, a lookup and three shifts per byte.
- **`kind_of`, made safe.** It transmuted a token word's kind field, trusting that every word was
  made by `word` or checked by `has_valid_kinds`. It is `SyntaxKind::from` now: one compare per
  lookup whose branch is never taken, and nothing to trust.
- **`SyntaxKind::from(u16)`, kept, moved** to `syntax_kind/raw.rs`. The safe version is a `match`
  of every kind or a table, which the generated code has neither of; a table is a load per
  conversion where the transmute costs the compare it makes anyway. Tested over every value and
  one past the last.
- **Untrusted input: source text.** The lexer and parser read any file of any dependency. A fuzz
  target `syntax_parse` is filed with safety (`v2/requests/safety.md`, "From
  speed-unsafe-front-syntax"; queue 00798): tokens tile the text, the fast path lexes as
  `rustc_lexer` alone, the tree gives back the text, bounded allocation, seeded with the parser's
  test files. For it, `parser` has a `fuzz` feature with `LexedStr::lexes_alike`.

## syntax

| | before | after |
|---|---|---|
| `src/raw.rs` | 3 | 2 |
| `examples/trees.rs` | 5 (root ungated) | 5, in `mod counting` |

- **`raw.rs`, kept, smaller.** A text built straight into its `Arc<str>` allocation: the safe way
  (`String`, then `Arc::<str>::from`) copies the file twice and allocates twice per edit. The copy
  into the uninitialized bytes is now the safe `<[MaybeUninit<u8>]>::write_copy_of_slice` (the same
  `memcpy`, stable since 1.93); left are `assume_init` and the `[u8]` to `str` cast. Reference tests
  (`concat`, and a `String` the edits are made in) were there.
- **`examples/trees.rs`**: a counting `GlobalAlloc` (real unsafe, the example's measurement). Its
  root now says `#![deny(unsafe_code)]` and the allocator is `#[allow(unsafe_code)] mod counting`.
  `tests/view.rs` says `#![forbid(unsafe_code)]`. `src/view/tests.rs`'s hit is Rust inside a string.
- **`crates/syntax/fuzz/`** is rust-analyzer's cargo-fuzz package (its own workspace, built by
  cargo and rustc, not by blaze); `syntax_parse` supersedes it, after which it can go. Its two
  targets say `#![forbid(unsafe_code)]`: `fuzz_target!` is `libfuzzer-sys`'s macro, and
  `unsafe_code` is not reported in macros defined in another crate. None of the three crates
  has a macro of its own that expands to `unsafe`.
