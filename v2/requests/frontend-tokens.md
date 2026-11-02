# Requests to frontend-tokens

## dedup: LitKind's wire number, once (from dedup-macros, 2026-10-29)

`LitKind` to and from its number 0 to 10 is written out in `crates/tt/src/storage.rs`
(`Node::literal` 144-156 and `Node::lit_kind` 166-178), in `hir-def`'s `nameres/frozen.rs`
(960-972 and 2289-2301, requested of db) and in both proc-macro wires (which move onto tt's
column bytes in your request to usercode). Design: `docs/v2/plan/lanes/dedup/macros.md`.

1. In `crates/tt/src/lib.rs`, after `pub enum LitKind`:

```rust
impl LitKind {
    /// This kind's number in tt's encodings (the column format, and hir-def's frozen trees), and
    /// its count of `#`s.
    #[inline]
    pub const fn code(self) -> (u8, u8) {
        match self {
            LitKind::Byte => (0, 0),
            LitKind::Char => (1, 0),
            LitKind::Integer => (2, 0),
            LitKind::Float => (3, 0),
            LitKind::Str => (4, 0),
            LitKind::StrRaw(hashes) => (5, hashes),
            LitKind::ByteStr => (6, 0),
            LitKind::ByteStrRaw(hashes) => (7, hashes),
            LitKind::CStr => (8, 0),
            LitKind::CStrRaw(hashes) => (9, hashes),
            LitKind::Err(()) => (10, 0),
        }
    }

    /// The kind [`LitKind::code`] numbers `code`, with `hashes` for a raw one; `None` past 10.
    #[inline]
    pub const fn from_code(code: u8, hashes: u8) -> Option<LitKind> {
        Some(match code {
            0 => LitKind::Byte,
            1 => LitKind::Char,
            2 => LitKind::Integer,
            3 => LitKind::Float,
            4 => LitKind::Str,
            5 => LitKind::StrRaw(hashes),
            6 => LitKind::ByteStr,
            7 => LitKind::ByteStrRaw(hashes),
            8 => LitKind::CStr,
            9 => LitKind::CStrRaw(hashes),
            10 => LitKind::Err(()),
            _ => return None,
        })
    }
}
```

2. `crates/tt/src/storage.rs`, `Node::literal`: replace the `let (code, hashes) = match kind { ... };`
   with `let (code, hashes) = kind.code();` and then `let (code, hashes) = (u32::from(code), hashes);`
   (the `head` expression shifts `code` as a `u32`).
   `Node::lit_kind`: replace its body with:

```rust
        let hashes = (self.head >> 6) as u8;
        // A node's code is one `literal` wrote, so it is at most 10; 11 to 15 read as `Err`, as
        // before.
        LitKind::from_code(((self.head >> 2) & 0xF) as u8, hashes).unwrap_or(LitKind::Err(()))
```

3. Tests in `crates/tt/src/storage/tests.rs`:
   - every kind, with 0, 1 and 255 hashes for the raw ones, round-trips through `code` and
     `from_code`;
   - `from_code(11..=255, _)` is `None`;
   - a `proptest` differential against the old `match`, copied into the test as the reference.
     This needs `proptest.workspace = true` under `[dev-dependencies]` in `crates/tt/Cargo.toml`;
     `proptest` is already a root workspace dependency.

> forwarded to frontend-tokens (`crates/tt/` is held by it, still running; told).
> done by frontend-tokens: items 1-3 as written (`LitKind::code`/`from_code` in `lib.rs`, `Node::literal`/`lit_kind` in `storage.rs` on them, `proptest.workspace = true` in tt's dev-dependencies); tests `a_literal_kind_round_trips_through_its_code` and the proptest `literal_codes_are_the_old_ones` in `storage/tests.rs`. manifest_lint clean. Nothing built.

## dedup: seeded RNG onto blaze_ids::rng

From `dedup-codec-rng-front` (report `/tmp/blaze-lanes/v2-dedup-codec-rng-front.md`). Every seeded test RNG in the front-end crates moves onto `blaze_ids::rng::SplitMix64` (`new`, `next_u64`, `next_u32`, `below(u64)` = `% n` and 0 for 0, `index(usize)`, `one_in`, `coin`, `pick`, `fill`); no test here pins a stream's values, only thresholds, so the local copy goes.

`crates/syntax-bridge/src/tests.rs` and `crates/mbe/src/differential.rs` are already moved onto `blaze_ids::rng::SplitMix64`, so both crates need the dev-dependency to build their tests:

- `crates/syntax-bridge/Cargo.toml`: add `blaze-ids.workspace = true` under `[dev-dependencies]` (in sorted place).
- `crates/mbe/Cargo.toml`: add `blaze-ids.workspace = true` under `[dev-dependencies]` (in sorted place).

Then run `PYTHONPATH=python/typed-input/src python3 tools/v2/manifest_lint.py '{"root": "."}'`.

> forwarded to frontend-tokens (from requests, pass 16; told by tmux).
> done by frontend-tokens: `blaze-ids.workspace = true` under `[dev-dependencies]` in syntax-bridge's and mbe's Cargo.toml; manifest_lint clean.

## frontend-tree-span: `SpanMap::push_fragment` takes the group's span (db-expand task 16, item 1)

`crates/span/src/map.rs` changed (frontend-tree-span, 2026-10-29):

- `pub fn push_fragment(&mut self, range: TextRange, span: Span)` (was `push_fragment(range)`):
  `span` is the invisible group's, which is its metavariable's (`id` after `marker` in
  `expand_var`). Of fragments with one range, push the innermost first, as a group closes.
- `pub fn fragment_span(&self, range: TextRange) -> Option<Span>`: the span of the fragment
  whose range is exactly `range` (of several, the innermost); kept through `merge`.

`syntax-bridge` does not compile until its one caller passes the span:
`crates/syntax-bridge/src/lib.rs`, `TtTreeSink::close_fragment`, today
`self.token_map.push_fragment(TextRange::new(start, self.token_end));`. The span is the invisible
subtree's delimiter: `subtree.delimiter.open` where `open_fragments` is pushed (the `None =>` arm
of `delim_to_str(subtree.delimiter.kind, false)`), equal to `parent.delimiter.close` where the
cursor leaves it. `finish` closes groups the cursor never left, so the span has to travel with the
start on the stack: `open_fragments` holds the start and the span (or a second stack of spans
beside the lent `u32`s), and `close_fragment` pushes `(TextRange::new(start, self.token_end),
span)`. A group closed with `start == token_end` still pushes nothing.

Then, in `crates/mbe/src/expander/transcriber.rs` `expand_var`, the first bullet of task 16:
`stmt` and `block` fragments wrapped in an invisible group spanned by the metavariable, as
`Fragment::Expr`/`Path`/`Ty` are, so their spans reach the map this way. hir-expand's side
(`map_node_range_up*` reading `fragment_span`) is requested of db-expand in
`v2/requests/db-expand.md`, `## frontend-tree-span: hir-expand's map_node_range_up*`.

Status: open

> forwarded to frontend-tokens (frontend-tokens-next1, which holds `syntax-bridge` and `mbe`; told by tmux; requests, pass 22).
> mem-format-expand, 2026-10-29: the sink half of this (`open_fragments` carrying the group's span, `close_fragment` passing it) is merged with mem-db-def-expand's F3 into one change on the same `TtTreeSink` lines: `v2/requests/mem-format-expand.md` section 3b. Apply that form instead of a separate edit; the `expand_var` half here is unchanged.
> done by frontend-tokens (2026-10-29). Sink: `open_fragments` is `SmallVec<[(TextSize, Span); 4]>`
> (the start and `subtree.delimiter.open`, so `finish` has the span too). `close_fragment` pushes
> `(TextRange::new(start, token_end), span)` and still pushes nothing when `start == token_end`.
> The rest of 3b is in as well (answered there). `expand_var`: `stmt` and `block` go out in an
> invisible group spanned by the metavariable (`TokensOrigin::Grouped`). Details are in
> `v2/requests/db-expand.md` under task 16, and the test is
> `a_stmt_and_a_block_go_out_in_groups_spanned_by_their_metavariables` (checks `fragment_span`).
> Status: done. Not built.

## dedup: byte cursor onto blaze_format::cursor (tt wire's writer)

From `dedup-cursor` (report `/tmp/blaze-lanes/v2-dedup-cursor.md`), routed here from
`v2/requests/usercode-wire-macros.md`, which named tt's paths by mistake. The reader half of that
request (`Words` holding a `blaze_format::cursor::Reader`, tt's `blaze-format.workspace = true`) is
already in the tree. What is left is the writer:

- `crates/tt/src/storage/wire.rs`: `use blaze_format::cursor::Reader;` becomes
  `use blaze_format::cursor::{Put as _, Reader};`, and in `put_rows`
  `out.extend_from_slice(&word.to_le_bytes());` becomes `out.put_u32(word);`.

Same bytes.

> forwarded to frontend-tokens (frontend-tokens-next1, holds `crates/tt/`; told by tmux; requests, pass 23).
> done by frontend-tokens (2026-10-29), as filed: `use blaze_format::cursor::{Put as _, Reader};`
> and `out.put_u32(word)` in `put_rows`. The bytes are the same (`Put for Vec<u8>` writes little-endian). Not built.
