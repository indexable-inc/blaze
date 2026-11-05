# la-arena

- Upstream: https://github.com/rust-lang/rust-analyzer (`lib/la-arena`), crates.io `la-arena`
  0.3.1 (sha256 3752f229dcc5a481d60f385fa479ff46818033d881d2d801aa27dffcfb5e8306).
- Licence: MIT OR Apache-2.0 (the published crate ships no licence file).
- Left out: `Cargo.toml.orig`; `Cargo.toml` is the published one less its generated header.

Changes, in src/lib.rs (src/map.rs is as published):

1. `RawIdx` holds `NonZeroU32` of the index's complement (`!raw`), so `Option<RawIdx>` and
   `Option<Idx<T>>` are 4 bytes, not 8 (speed-hir, queue request 00068). `from_u32`,
   `into_u32`, `From<u32>`, `Debug`, `Display`, `Ord` and `Hash` keep their meaning: order and
   hash are those of the index. `u32::MAX` is the one index refused: `from_u32(u32::MAX)` panics
   naming it.
2. `Arena` holds at most `u32::MAX - 1` values (`alloc` and `FromIterator` panic past it), so
   every index and every range end it gives is below `u32::MAX`. Upstream truncated an index
   past `u32::MAX` silently.
3. Tests at the end of src/lib.rs check both.
