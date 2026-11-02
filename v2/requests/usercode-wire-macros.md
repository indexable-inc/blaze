
## dedup: LEB128 and seeded RNGs onto `blaze_format::leb` and `blaze_ids::rng`

From `dedup-codec-leb` (report `/tmp/blaze-lanes/v2-dedup-codec-leb.md`). The one LEB128 is
`crates/blaze-format/src/leb.rs`; its API is listed in `v2/requests/mem-backend-link.md` under
the same heading. The RNG is `blaze_ids::rng::SplitMix64` (`crates/blaze-ids/src/rng.rs`).

- `crates/blaze-macro-wasm/Cargo.toml`: add `blaze-format.workspace = true` to
  `[dependencies]` (it depends only on `blaze-ids`, `serde`, `zstd`: no cycle).
- `src/lib.rs` `carried_list`: the nested `fn leb` becomes
  ```rust
      fn leb(bytes: &[u8], at: &mut usize) -> Option<usize> {
          blaze_format::leb::read_u32(bytes, at).ok().map(|it| it as usize)
      }
  ```
  (wasm's `u32`: at most 5 bytes, as the loop read, and now also refusing bits past 32).

> forwarded to usercode-wire-macros (from requests, pass 16; told by tmux).
> applied (usercode): `blaze-format` in `blaze-macro-wasm`'s `[dependencies]`; `carried_list`'s `leb` is `blaze_format::leb::read_u32`. The `blaze-expand` dev-dependency below is already in its manifest.
> applied (usercode-wire-macros): the seeded RNG half: `native/tests.rs`'s own xorshift `Rng` is gone; its random trees draw from `blaze_ids::rng::SplitMix64` (`index`). No other generator or LEB128 is left in `blaze-macro-wasm`, `blaze-expand/src/bridge` or `native/`.

## usercode-wire-macros: what the column wire needs outside its paths

From `usercode-wire-macros` (report `/tmp/blaze-lanes/v2-usercode-wire-macros.md`), items 3
and 4 of `v2/requests/usercode.md` `## frontend-tokens`.

- `crates/blaze-macro-wasm/Cargo.toml` (usercode): `ra-ap-rustc_lexer` is no longer used by the
  crate: its lexer is `blaze_expand::bridge::lex` now. Remove the line.
- `fuzz/targets/macro_wasm_wire.rs` (fuzz lane): the module now writes, and the host reads,
  tt's columns (`TopSubtree::write_wire` layout, each span part named `{file_id: index,
  ast_id: 0, ctx: 0}` with the sites at indices 0..=2, read with `read_wire`). Its `Wire`
  writer, seeds, dictionary and the "2 trees per 15 bytes" bound are of the old stream format
  and want redoing: seeds from `write_wire` of lexed trees with the part rows rewritten, and a
  bound of one token tree per 16 bytes (a node is 2 words and its span 2). `fuzz/README.md`'s
  row with it. `HOST_VERSION` is now `blaze v2 wasm proc-macro host 2`.
- `tools/v2/hot_lint.py`: `{"crates": [...]}` fails with `typed_input: crates has a type it
  does not read: list[str]`, so a crate outside `HOT` can't be linted.
- Not done here, outside these paths: the forwarded `## dedup: LEB128` request above
  (`blaze-macro-wasm/Cargo.toml` and `carried_list`, neither of which this lane owns); back to
  usercode-wire.
- `crates/blaze-macro-wasm/src/lib.rs` (usercode-wire): on a store hit `expand` still
  allocates before its lookup in `guest_env` (a `collect` per call) and `package_dir`; the
  rest of a hit (span parts, shapes, input and attribute bytes, the kept output's parts) now
  reuses this thread's room (`Scratch`, `Reused`), so those two are what's left besides the
  output tree.

> forwarded to usercode-wire-macros (from requests, pass 17; told by tmux).
> applied (usercode-wire-macros): `ra-ap-rustc_lexer` removed from `blaze-macro-wasm/Cargo.toml`
> (manifest lint passes). Moot: `hot_lint.py` takes `{"crates": [...]}` now; the LEB128 item was
> applied (see its section). Still open, not this lane's paths and past its context: the fuzz
> target `fuzz/targets/macro_wasm_wire.rs` and its README row (fuzz lane), and the `guest_env` /
> `package_dir` allocations on a hit (usercode-wire, `blaze-macro-wasm/src/lib.rs`).
> forwarded (usercode-wire): `src/lib.rs` is not usercode-wire's. The `guest_env` / `package_dir` item is `v2/requests/usercode-wire.md` section 6, which usercode forwarded to usercode-hot-macros (the lane holding `crates/blaze-macro-wasm/`).

## dedup: byte cursor onto blaze_format::cursor

From `dedup-cursor` (report `/tmp/blaze-lanes/v2-dedup-cursor.md`). tt's new wire reader
(`crates/tt/src/storage/wire.rs`, `struct Words`) is a byte cursor; read it with
`blaze_format::cursor::Reader` (one bounds compare and unaligned load a read, error naming the
byte), keeping `rows`, `column` and `pad`, which are the wire's own.

- `crates/tt/Cargo.toml` `[dependencies]`: `blaze-format.workspace = true` (already in tt's graph
  through `intern`).
- `crates/tt/src/storage/wire.rs`, imports: add `use blaze_format::cursor::Reader;`.
- `struct Words<'a> { bytes: &'a [u8], at: usize }` becomes `struct Words<'a> { read: Reader<'a> }`.
- `Words::bytes` becomes:

  ```rust
  fn bytes(&mut self, len: usize) -> Result<&'a [u8], WireError> {
      self.read.take(len).map_err(|error| WireError::Truncated { at: error.at, needed: len })
  }
  ```
- `Words::column`: `let at = self.at;` becomes `let at = self.read.at();`.
- `Words::pad`: `self.at.next_multiple_of(4) - self.at` becomes
  `self.read.at().next_multiple_of(4) - self.read.at()`, and `malformed("padding that is not
  zero", self.at)` becomes `malformed("padding that is not zero", self.read.at())`.
- `read_wire`: `let mut words = Words { bytes, at: 0 };` becomes
  `let mut words = Words { read: Reader::new(bytes) };`; `let mut ends = Words { bytes, at:
  words.at };` becomes `let mut ends = Words { read: words.read };` (`Reader` is `Copy`);
  `Ok((tree, words.at))` becomes `Ok((tree, words.read.at()))`.

Same bytes accepted and refused, same `WireError`s and offsets.

> not this lane (usercode-wire-macros): every edit is in `crates/tt`, which is frontend-tokens'; this lane only asks for tt changes. Back to the requests pass to route to frontend-tokens.

> forwarded to frontend-tokens (frontend-tokens-next1, editing `crates/tt/`; format and usercode-wire-macros are DONE) (requests, pass 18; told by tmux).
> done by frontend-tokens: every edit as written, against the current `wire.rs` (its `Words` was unchanged by the block rewrite). `blaze-format.workspace = true` in tt's `[dependencies]` (no cycle: blaze-format depends on blaze-ids, serde, zstd). `Reader::take`'s error `at` is the cursor position before the read, as `Truncated { at }` was. manifest_lint clean, hot_lint nothing in `wire.rs`. Nothing built.

> dedup-cursor: routed to `v2/requests/frontend-tokens.md` (tt is frontend-tokens'); the reader
> half is already applied, the writer half (`put_rows`) is requested there.

## frontend-tokens: two doc links name the moved `write_wire`

`tt::TopSubtree::write_wire` is now `tt::Tree::write_wire` (`TopSubtree<A>` derefs to the
non-generic `Tree`, which holds the readers and wire writers). Two module docs still link the old
path:
- `crates/proc-macro-api/src/wire.rs:2`: `[`tt::TopSubtree::write_wire`]` should become `[`tt::Tree::write_wire`]`.
- `crates/blaze-expand/src/bridge/wire.rs:3`: `` `tt::TopSubtree::write_wire` `` should become `` `tt::Tree::write_wire` ``.
These are doc text only, with no code change.

> forwarded to usercode (`proc-macro-api`, `blaze-expand`; usercode-next1); told by tmux (requests, pass 26).

> applied (usercode, 2026-10-29): `proc-macro-api/src/wire.rs:2` now links `tt::Tree::write_wire`. `blaze-expand/src/bridge/wire.rs:3` already said `tt::Tree::write_wire`. `read_wire` on the same line stays `tt::TopSubtree::read_wire`, where it still is. Doc text only.
