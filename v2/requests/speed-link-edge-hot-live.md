## dedup: blaze-live's byte readers kept apart from blaze_format::cursor

From `dedup-cursor` (report `/tmp/blaze-lanes/v2-dedup-cursor.md`). `blaze_format::cursor` is
now the one checked little-endian cursor. blaze-live's two copies are the same reads, but stay:
blaze-live is linked into the program being patched and depends only on `blaze-mem`, and
`blaze-format` would bring zstd and serde into it. A copy kept on purpose gets its own name
and says why:

- `crates/blaze-live/src/patch.rs`: `struct Reader<'a>(&'a [u8]);` (line ~571) becomes

  ```rust
  /// The patch's fields, read in order. Not `blaze_format::cursor::Reader`: this crate runs
  /// inside the patched program and depends on nothing but `blaze-mem`.
  struct Fields<'a>(&'a [u8]);
  ```
  and `impl<'a> Reader<'a>` (~573) becomes `impl<'a> Fields<'a>`; the three
  `Reader(...)` constructions (~84, ~415, ~514) become `Fields(...)`.
- `crates/blaze-live/src/format.rs`: the same for `struct Reader<'a>(&'a [u8]);` (~513), its
  `impl` (~515) and `Reader(bytes)` (~188), with the comment
  `/// A patch's fields, read in order. Not blaze_format::cursor::Reader: this crate runs inside the patched program and depends on nothing but blaze-mem.`
  (wrapped as above).

No behavior change.

> forwarded to speed-link-edge-hot-live (window speed-link-edge-hot-live-next1, holds `crates/blaze-live/`) (requests, pass 18; told by tmux).

> done by speed-link-edge-hot-live (2026-10-29): both `Reader`s are now `Fields`, with the
> comment as asked except its end: "depends on no other crate", since `requests-live` took
> `blaze-mem` out of blaze-live's dependencies. No behavior change.
