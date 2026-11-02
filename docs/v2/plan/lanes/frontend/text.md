---
status: plan
summary: blaze v2 keeps each file's text as the VFS's own bytes (checked to be UTF-8 once per content, never copied) and finds lines and columns only when a diagnostic, line!/column!, a caller location or debug info asks, from a SIMD newline bitmap with a rank table (frontend.md item 6).
date: 2026-10-29
---
# blaze v2: mapped file text and a lazy newline index (frontend-text)

Design notes of lane `frontend-text` for item 6 of [the front end's design](../../design/frontend.md),
as written on 2026-10-29. Nothing was built or run: every claim below is what the code is written
to do, and each test named is what proves it once it runs.

## What changes

Before: every file's text was copied out of the VFS into an `Arc<str>` on load and kept, and
lines were found by scalar scans into `Vec<u32>`s of line starts in several places: hir-expand's
`line_starts` (`line!`/`column!`), hir-ty-mir's `line_starts` (caller locations and debug-info
rows), hir-ty-codegen's `anchor_rows` (rows inside a body), load-cargo's and ide-db's
`line_index` crate (proc macros' `Span::line`/`column`, blaze-engine's test and doctest
lines), and blaze-diagnostics' `emit.rs` (memchr).

After:

- **Text is the VFS's bytes.** `text_of(Bytes) -> Result<Text, Utf8Error>`
  (`crates/blaze-bytes`, re-exported by blaze-vfs and, as `Text`, by `syntax-tree` and `syntax`)
  checks a file's `Bytes` (a heap file's `Arc<[u8]>`, a range of the mapped image, or a
  `'static` slice) are UTF-8 once. A `Text` is those bytes, lent as `&str`. `Text::shared()`
  gives a heap file's `Arc<str>`, the same allocation with one more count, and
  `Text::from(Arc<str>)` is the reverse, with no copy. `Reader::text` and `Vfs::text`
  (`crates/blaze-vfs/src/text.rs`) read a file that way. base-db's view install (`write_view`)
  calls it for each file whose content changed, so a text is checked once per content ID and
  never copied.
- **The parse tree's source is the same `Text`.** `FlatTree::source()` is a `Text`, and so is
  each lazy part's (`Expand` builds the part over the file's text with `sourced_range`). The
  parse functions take `impl Into<Text>`. Once `FileText` and hir-def's `BodySyntax` hold a
  `Text` (requested of db), a file restored from the image goes from the mapped bytes to the
  parse with no copy, and nothing has to be refused.
- **One line index per text, built on first use.** `span::LineIndex`
  (`crates/span/src/lines.rs`) is made by the base-db query `line_index(db, FileText)`, which
  nothing calls on load: only a diagnostic, `line!`/`column!`, a caller location, a proc macro's
  `Span::line`/`column` or debug info does. Salsa keys it by the `FileText` input, which changes
  only with the content.

## The index

- **Bitmap**: bit `i % 64` of word `i / 64` is set when byte `i` is `\n`, made 64 bytes at a
  time (`crates/span/src/lines/simd.rs`): one AVX-512BW compare into a mask register, two AVX2
  compares and mask moves, four SSE2 ones, or four NEON compares folded with three pairwise adds
  of weighted lanes. `Kernel::best()` chooses at run time; `Kernel::Scalar` is the reference.
- **Rank table**: the count of newlines before each 512 bytes (one cache line of bits), one
  `u32` each, plus the total. This is rank9's sampling without its relative counts: 0.125 +
  0.0078 = 0.133 bytes of index per byte of text, against a `Vec<u32>` of line starts' 4 bytes a
  line (0.1 bytes per byte at 40-byte lines), and it is built in the same pass.
- **offset -> line**: one table load and at most eight popcounts in one cache line.
  **line start**: a backward scan of the words of that block, or a select. **line -> offset**
  (`line_start`, `line_end`): a binary search of the rank table, popcounts in one cache line,
  then a bit select in one word.
- **Columns are rustc's** (`rustc_span::SourceFile`, `SourceMap::lookup_char_pos`). A leading
  byte order mark counts as no column. A `\r` before a `\n` is no column of the `\n`. A `\n` that
  ends the text starts no line. `Place` gives each column rustc uses: chars (`column!`, a
  diagnostic's `file:line:col`, `proc_macro::Span::column`, libtest's test positions), display
  width with rustc's `char_width` (`Location::caller`, `col_display`, which debug-info rows use
  today), and bytes (what rustc's LLVM back end writes to DWARF's line table).
- `row(offset)` is for a text that is part of a file (a body's text): rows are newlines before,
  with none of the whole-file rules.

## State of the art, and where this stands

- **rustc** `analyze_source_file`: SSE2 16 bytes at a time, recording each line start into a
  `Vec` and each multibyte char into another, eagerly for every file it loads. Here 64 bytes go
  per step with the widest unit, nothing is recorded per line, and nothing is built until asked.
- **simdjson stage 1** (structural bitmaps from compares and mask moves, 64 bytes a word): the
  bitmap is made the same way, with one character class.
- **rank9 / poppy** (Vigna 2008; Zhou, Andersen and Kaminsky 2013) for rank and select over
  bitmaps: the table is rank9's top level, at one entry per cache line of bits, so rank is one
  table load plus one cache line of the bitmap.
- **rust-analyzer's `line-index`**: a `Vec` of line starts plus wide-char tables, built eagerly
  per query, with byte columns. It is replaced, and its byte columns, which were wrong for
  `proc_macro::Span::column` after a non-ASCII char, go with it.
- **simdutf** for UTF-8 checks: here an all-ASCII prefix is skipped 64 bytes at a time by an OR
  of eight words, which vectorizes to a load and a test per 32 or 64 bytes. std checks what
  follows the first non-ASCII chunk. Source is nearly all ASCII, so that tail is rare.

## Speed of light

A text of N bytes on one core:

- **Building the bitmap** reads N bytes and writes N/8. Per 64 bytes, AVX2 runs about 8
  instructions (2 loads, 2 compares, 2 mask moves, a shift-or, a store) and NEON about 17. At 4
  to 8 per cycle that is 32 B/cycle on x86 and about 30 B/cycle on an M-series core, roughly
  100-150 GB/s at their clocks. That is above what one core streams from L3 or DRAM (about 20-30
  B/cycle on x86, about 100 GB/s on M-series), so building is bound by memory bandwidth, the
  floor for any method that reads the text. The rank pass is 8 popcounts and an add per 512
  bytes, about 1/40th of the compare work, over bits already in L1. Bound: N / (single-core
  read bandwidth). Design: within 10% of it, with the rank pass as the only extra work.
- **The UTF-8 check** on ASCII text is the same kind of stream (an OR and a test per 64 bytes),
  so it is bound by the same bandwidth. Without the copy, the text costs no memcpy of N bytes and
  no second N bytes resident. A text never asked for a line pays only this check.
- **A lookup** (offset -> line and line start) touches one `u32` of the table and one 64-byte
  line of bits: at most two cache misses, about 10 ns cold and a few ns warm. Its columns are a
  char count of the line before the offset, which std's `chars().count()` and the ASCII
  `display_width` path vectorize. line -> offset is a binary search over N/512 entries (8 steps
  for a 100 KB file, all in L1) and one cache line.
- **Memory**: 0.133 bytes per byte of text, only for texts something asked about.

## Hooks (requests)

| Where | What | Request |
| --- | --- | --- |
| `crates/blaze-vfs/src/lib.rs` | declare `text` (unsafe allowed), export `text_of` | `v2/requests/frontend-journal.md`, applied |
| `crates/blaze-vfs` (`store.rs`, `image.rs`, `lib.rs`, `Cargo.toml`) | `Bytes` is `blaze_bytes::Bytes`, image ranges through `Bytes::held`; export `Text`, `TextFile` | `v2/requests/frontend-journal.md` |
| root `Cargo.toml` | `blaze-bytes` workspace dependency | `v2/requests/frontend.md` |
| `crates/base-db`, `crates/hir-def` | `FileText` and `BodySyntax` hold a `Text`; two callers of the tree's source | `v2/requests/db.md` |
| `crates/base-db` | `line_index` query; `ViewFile` carries the tree's `Arc<[u8]>`; `FileText` from `text_of` | `v2/requests/db.md` |
| `crates/hir-expand` | `line_and_column` on `line_index`, `line_starts` removed | `v2/requests/db.md` |
| `crates/load-cargo` | `set_files`' `ViewFile`; `line_column` on `line_index` | `v2/requests/cargo.md` |
| `crates/hir-ty-mir`, `crates/hir-ty-codegen`, `crates/blaze-engine` | caller locations, debug-info rows, body rows, test and doctest lines | `v2/requests/codegen.md` |
| `crates/blaze-diagnostics/src/emit.rs` | `File`'s line starts on `LineIndex` | `v2/requests/diagnostics.md` |

## What proves it

In `crates/span/src/lines/tests.rs`:

- `every_kernel_gives_the_scalar_bits`: every kernel this CPU runs gives the scalar kernel's
  words, for every length and alignment. `every_kernel_builds_the_same_index` does the same for
  whole indexes over random texts.
- `places_are_rustcs`: at every char boundary of random texts weighted to the hard cases
  (`\r\n`, lone `\r`, BOMs, tabs, controls, wide and multibyte chars, lines crossing rank
  blocks), line and all three columns equal a model of rustc's `SourceFile` (BOM dropped, `\r\n`
  normalized, its line starts and char positions).
- `lines_are_where_newlines_split_the_text`: line starts and ends against `split('\n')`.
- `places_match_the_old_line_starts`: the differential against the removed hir-expand and
  hir-ty-mir code. The two agree everywhere except at the three places the old code was not
  rustc's.
- `rows_are_newlines_before` (the differential for hir-ty-codegen's `anchor_rows`),
  `rustcs_columns_by_example`, `columns_need_a_char_boundary`,
  `ascii_display_width_is_char_widths`, `the_index_costs_an_eighth_of_the_text`.

In `crates/blaze-bytes/src/tests.rs` and `crates/blaze-vfs/src/text.rs`:

- `text_shares_the_bytes` and `a_tree_file_is_read_as_its_own_bytes`: the text is the tree's
  allocation (same pointer, count moved), through `text_of`, `Reader::text` and `Vfs::text`.
- `held_bytes_keep_their_owner`, `an_arc_str_is_text_as_it_is`,
  `static_bytes_are_neither_shared_nor_held`: each kind of owner, and `Arc<str>` both ways.
- `text_allocates_nothing` and `reading_text_allocates_nothing`: making, cloning, sharing and
  reading text allocate nothing.
- `bytes_that_are_not_utf8_say_where`: a refusal reports where the bytes stop being UTF-8 and
  drops its count.
- `the_check_is_stds`: the ASCII-prefix check gives std's `from_utf8` result, error included,
  on random bytes. `the_ascii_prefix_stops_at_the_chunk_with_a_high_byte` checks the prefix.
